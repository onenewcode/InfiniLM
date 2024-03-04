mod cache;
mod kernel;
mod storage;

use common::{upos, utok};
use gemm::f16;
use kernel::{gather, mat_mul, rms_norm, rms_norm_inplace, rotary_embedding, softmax, swiglu};
use model_parameters::{Llama2, Memory};
use storage::Storage;
use tensor::{reslice, reslice_mut, slice, udim, DataType, Tensor};

pub use cache::LayerCache;
pub extern crate model_parameters;

pub struct Transformer {
    model: Box<dyn Llama2>,
}

pub struct Request<'a> {
    pub tokens: &'a [utok],
    pub cache: &'a mut [LayerCache],
    pub pos: upos,
}

impl Request<'_> {
    #[inline]
    pub const fn seq_len(&self) -> udim {
        self.tokens.len() as udim
    }

    #[inline]
    pub const fn att_len(&self) -> udim {
        self.pos + self.seq_len()
    }
}

impl Transformer {
    #[inline]
    pub fn new(model: Box<dyn Llama2>) -> Self {
        Self {
            model: match model.data_type() {
                DataType::BF16 | DataType::F32 => Box::new(Memory::cast(&*model, DataType::F16)),
                _ => model,
            },
        }
    }

    #[inline]
    pub fn new_cache(&self) -> Vec<LayerCache> {
        LayerCache::new_layers(&*self.model)
    }

    #[inline]
    pub fn max_seq_len(&self) -> usize {
        self.model.max_position_embeddings()
    }

    pub fn update(&self, requests: &mut [Request]) -> Tensor<Storage> {
        // println!("tokens:");
        // for request in requests.iter() {
        //     println!(
        //         "{:?}: {:?}",
        //         request.tokens,
        //         request.pos..request.pos + request.tokens.len() as upos
        //     );
        // }

        let mut nt = 0 as udim;
        let mut max_seq_len = 0 as udim;
        let mut max_att_len = 0 as udim;
        for request in requests.iter() {
            let seq_len = request.seq_len();
            let att_len = request.att_len();
            nt += seq_len;
            max_seq_len = max_seq_len.max(seq_len);
            max_att_len = max_att_len.max(att_len);
        }
        let nt = nt;
        let max_seq_len = max_seq_len;
        let max_att_len = max_att_len;

        let d = self.model.hidden_size() as udim;
        let nh = self.model.num_attention_heads() as udim;
        let nkvh = self.model.num_key_value_heads() as udim;
        let dh = d / nh;
        let dkv = nkvh * dh;
        let head_group = nh / nkvh;
        let head_div = (dh as f32).sqrt().recip();
        let di = self.model.intermediate_size() as udim;
        let dt = self.model.data_type();
        let epsilon = self.model.rms_norm_eps();
        let theta = self.model.rope_theta();
        let mut pos = Vec::<u32>::with_capacity(nt as usize);
        for request in requests.iter() {
            let len = request.tokens.len() as u32;
            pos.extend(request.pos..(request.pos + len));
        }
        let pos = Tensor::new(DataType::U32, &[nt], reslice(&pos));

        let mut x0 = tensor(dt, &[nt, d]);
        let mut x1 = tensor(dt, &[nt, d]);
        let mut qkv = tensor(dt, &[nt, d + dkv + dkv]);
        let mut q_buf = vec![0u8; (nh * max_seq_len * dh) as usize * dt.size()];
        let mut att_buf =
            vec![0u8; (nkvh * head_group * max_seq_len * max_att_len) as usize * dt.size()];
        //                         `num_token x hidden_size`
        // -|reshape|------------> `num_token x (num_kv_head x head_group x head_dim)`
        // -|transpose(1,2,0,3)|-> `num_kv_head x head_group x num_token x head_dim`
        // -|reshape|------------> `num_kv_head x (head_group x num_token) x head_dim`
        let mut x2 = tensor(dt, &[nkvh, head_group * nt, dh]);
        let mut gate_up = tensor(dt, &[nt, di + di]);

        gather(x0.access_mut(), &self.model.embed_tokens(), requests);
        // println!("gather:\n{}", x0.access());

        for layer in 0..self.model.num_hidden_layers() {
            let input_layernorm = self.model.input_layernorm(layer);
            rms_norm(x1.access_mut(), &x0.access(), &input_layernorm, epsilon);
            // println!("layer {layer} input norm:\n{}", x1.access());
            let w_qkv = self.model.w_qkv(layer).transpose(&[1, 0]);
            mat_mul(&mut qkv.access_mut(), 0., &x1.access_mut(), &w_qkv, 1.);
            let mut qkv = qkv.split(1, &[d as _, dkv as _, dkv as _]);
            let v = qkv.pop().unwrap().reshape(&[nt, nkvh, dh]);
            let mut k = qkv.pop().unwrap().reshape(&[nt, nkvh, dh]);
            let mut q = qkv.pop().unwrap().reshape(&[nt, nh, dh]);
            // println!("layer {layer} q:\n{}", q.access());
            // println!("layer {layer} k:\n{}", k.access());
            // println!("layer {layer} v:\n{}", v.access());
            rotary_embedding(q.access_mut(), &pos, theta);
            rotary_embedding(k.access_mut(), &pos, theta);
            // println!("layer {layer} rot q:\n{}", q.access());
            // println!("layer {layer} rot k:\n{}", k.access());
            let q = q.transpose(&[1, 0, 2]);
            let k = k.transpose(&[1, 0, 2]);
            let v = v.transpose(&[1, 0, 2]);

            let mut req = 0;
            for r in requests.iter_mut() {
                let pos = r.pos;
                let seq_len = r.seq_len();
                let att_len = r.att_len();

                let req_slice = &[slice![all], slice![from req, take seq_len], slice![all]];
                let cat_slice = &[slice![all], slice![from pos, take seq_len], slice![all]];
                let att_slice = &[slice![all], slice![from   0, take att_len], slice![all]];
                req += seq_len;

                let q = q.clone().slice(req_slice);
                let k = k.clone().slice(req_slice);
                let v = v.clone().slice(req_slice);

                let (k_cache, v_cache) = r.cache[layer].get();
                let mut q_att = Tensor::new(dt, &[nh, seq_len, dh], q_buf.as_mut_slice());
                let mut k_cat = k_cache.clone().slice(cat_slice);
                let mut v_cat = v_cache.clone().slice(cat_slice);
                q.access().reform_to(&mut q_att);
                k.access().reform_to(&mut k_cat.access_mut());
                v.access().reform_to(&mut v_cat.access_mut());

                let q_att = q_att.reshape(&[nkvh, head_group * seq_len, dh]);
                let k_att = k_cache.clone().slice(att_slice);
                let v_att = v_cache.clone().slice(att_slice);
                // println!("layer {layer} q attention:\n{}", q_att.access());
                // println!("layer {layer} k attention:\n{}", k_att.access());
                // println!("layer {layer} v attention:\n{}", v_att.access());

                let mut att = Tensor::new(
                    dt,
                    &[nkvh, head_group * seq_len, att_len],
                    att_buf.as_mut_slice(),
                );
                {
                    let k_att = k_att.transpose(&[0, 2, 1]);
                    mat_mul(&mut att, 0., &q_att, &k_att.access(), head_div);
                    // println!("layer {layer} before softmax:\n{}", att.access());
                    att = att.reshape(&[nh, seq_len, att_len]);
                    softmax(&mut att);
                    // println!("layer {layer} after softmax:\n{}", att.access());
                    att = att.reshape(&[nkvh, head_group * seq_len, att_len]);
                    {
                        mat_mul(&mut x2.access_mut(), 0., &att, &v_att.access(), 1.);
                        let x2 = x2.clone().reshape(&[nh, seq_len, dh]).transpose(&[1, 0, 2]);
                        let mut x1 = x1.clone().reshape(&[seq_len, nh, dh]);
                        x2.access().reform_to(&mut x1.access_mut());
                    }
                    // println!("layer {layer} after attention:\n{}", x1.access());
                }
            }

            let wo = self.model.self_attn_o_proj(layer).transpose(&[1, 0]);
            mat_mul(&mut x0.access_mut(), 1., &x1.access(), &wo, 1.);
            // println!("layer {layer} o_proj:\n{}", x0.access());

            let post_layernorm = self.model.post_attention_layernorm(layer);
            rms_norm(x1.access_mut(), &x0.access(), &post_layernorm, epsilon);
            // println!("layer {layer} post norm:\n{}", x1.access());

            let w_gate_up = self.model.mlp_gate_up(layer).transpose(&[1, 0]);
            mat_mul(&mut gate_up.access_mut(), 0., &x1.access(), &w_gate_up, 1.);
            let mut gate_up = gate_up.split(1, &[di as _, di as _]);
            let up = gate_up.pop().unwrap();
            let mut gate = gate_up.pop().unwrap();
            // println!("layer {layer} gate:\n{}", gate.access());
            // println!("layer {layer} up:\n{}", up.access());

            swiglu(gate.access_mut(), unsafe { &up.access_unchecked() });
            // println!("layer {layer} swiglu:\n{}", gate.access());

            let mlp_down = self.model.mlp_down(layer).transpose(&[1, 0]);
            mat_mul(&mut x0.access_mut(), 1., &gate.access(), &mlp_down, 1.);
            // println!("layer {layer} down:\n{}", x0.access());
        }

        x0
    }

    pub fn decode(&mut self, requests: &mut [Request]) -> Vec<f16> {
        assert!(requests.iter().all(|r| r.seq_len() == 1));
        let batch = requests.len() as udim;

        let mut x = self.update(requests);

        let model_norm = self.model.model_norm();
        rms_norm_inplace(&mut x.access_mut(), &model_norm, self.model.rms_norm_eps());
        // println!("pos {pos} model norm:\n{}", x.access());

        let dt = self.model.data_type();
        let voc = self.model.vocab_size() as udim;
        let mut buf = vec![f16::ZERO; (batch * voc) as usize];
        let mut logits = Tensor::new(dt, &[batch, voc], reslice_mut(&mut buf));
        let lm_head = self.model.lm_head().transpose(&[1, 0]);
        mat_mul(&mut logits, 0., &x.access(), &lm_head, 1.);
        // println!("pos {pos} logits:\n{}", logits.access());

        buf
    }
}

fn tensor(dt: DataType, shape: &[udim]) -> Tensor<Storage> {
    Tensor::new(
        dt,
        shape,
        Storage::new(shape.iter().product::<udim>() as usize * dt.size()),
    )
}

#[test]
fn test_build() {
    use model_parameters::SafeTensorError;
    use std::{io::ErrorKind::NotFound, time::Instant};

    let t0 = Instant::now();
    let safetensors = Memory::load_safetensors_from_dir("../../TinyLlama-1.1B-Chat-v1.0");
    let t1 = Instant::now();
    println!("mmap {:?}", t1 - t0);

    let safetensors = match safetensors {
        Ok(m) => m,
        Err(SafeTensorError::Io(e)) if e.kind() == NotFound => return,
        Err(e) => panic!("{e:?}"),
    };

    let t0 = Instant::now();
    let _transformer = Transformer::new(Box::new(safetensors));
    let t1 = Instant::now();
    println!("build transformer {:?}", t1 - t0);
}
