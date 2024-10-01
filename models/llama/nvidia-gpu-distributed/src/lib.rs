#![cfg(detected_nccl)]

mod distribute;
mod parameters;

#[macro_use]
extern crate log;

use causal_lm::{CausalLM, DecodingMeta, Model, QueryContext, SampleMeta};
use common::{upos, utok, FileLoadError};
use common_nv::{
    cuda::{
        AsRaw, Context, ContextResource, ContextSpore, DevByte, DevMem, DevMemSpore, Device,
        HostMemSpore, Stream, StreamSpore,
    },
    nccl::{CommunicatorGroup, ReduceType},
    slice, split, udim, KernelsA, KernelsB, LocalSplitable, NvidiaKernels, Tensor,
};
use itertools::izip;
use llama::InferenceConfig;
use parameters::{Layer, ParameterMatrix};
use std::{
    iter::{repeat, zip},
    mem::{take, ManuallyDrop},
    path::Path,
    slice::{from_raw_parts, from_raw_parts_mut},
    sync::Arc,
    time::Instant,
};

pub use common_nv::cuda;

pub struct Transformer {
    config: InferenceConfig,

    comms: CommunicatorGroup,
    streams: Vec<StreamSpore>,
    kernels: NvidiaKernels,
    sample_workspace: ManuallyDrop<DevMemSpore>,

    embed_tokens: Tensor<ManuallyDrop<HostMemSpore>>,
    matrix: ParameterMatrix,
    lm_layernorm: Tensor<ManuallyDrop<DevMemSpore>>,
    lm_head: Tensor<ManuallyDrop<DevMemSpore>>,
}

impl Model for Transformer {
    type Meta = Vec<Device>;
    type Error = FileLoadError;

    #[inline]
    fn load(model_dir: impl AsRef<Path>, meta: Self::Meta) -> Result<Self, Self::Error> {
        let time = Instant::now();
        let host = llama::Storage::load_safetensors(model_dir)?;
        info!("load host: {:?}", time.elapsed());

        let kernels = NvidiaKernels::new(&meta, host.config.d as _, host.config.voc as _);

        let contexts = meta
            .iter()
            .map(|dev| {
                dev.set_mempool_threshold(u64::MAX);
                dev.retain_primary()
            })
            .collect::<Vec<_>>();
        let comms = CommunicatorGroup::new(
            &meta
                .iter()
                .map(|dev| unsafe { dev.as_raw() })
                .collect::<Vec<_>>(),
        );
        let matrix = ParameterMatrix::load(&host, &contexts);
        let streams = contexts
            .iter()
            .map(|context| context.apply(|ctx| ctx.stream().sporulate()))
            .collect::<Vec<_>>();
        let (embed_tokens, lm_layernorm, lm_head, sample_workspace) = contexts[0].apply(|ctx| {
            let sample_workspace = kernels
                .sample_workspace(streams[0].sprout_ref(ctx))
                .sporulate();

            (
                host.embed_tokens.map_physical(|u| {
                    let mut host = ctx.malloc_host::<u8>(u.len());
                    host.clone_from_slice(&u);
                    ManuallyDrop::new(host.sporulate())
                }),
                host.lm_layernorm
                    .map_physical(|u| ManuallyDrop::new(ctx.from_host(&u).sporulate())),
                host.lm_head
                    .map_physical(|u| ManuallyDrop::new(ctx.from_host(&u).sporulate())),
                ManuallyDrop::new(sample_workspace),
            )
        });
        Ok(Self {
            comms,
            streams,
            kernels,
            sample_workspace,

            embed_tokens,
            matrix,
            lm_layernorm,
            lm_head,

            config: host.config,
        })
    }
}

impl CausalLM for Transformer {
    type Storage = Cache;

    #[inline]
    fn max_seq_len(&self) -> upos {
        self.config.max_seq_len
    }
    #[inline]
    fn bos_token(&self) -> utok {
        self.config.bos_token
    }
    #[inline]
    fn eos_token(&self) -> utok {
        self.config.eos_token
    }

    fn new_cache(&self) -> Tensor<Self::Storage> {
        let contexts = Arc::new(self.comms.contexts().collect::<Vec<_>>());
        let n = contexts.len() as udim;
        let distributed = InferenceConfig {
            nkvh: self.config.nkvh / n,
            ..self.config.clone()
        };

        distributed.new_cache(|len| Cache {
            mem: contexts
                .iter()
                .map(|context| context.apply(|ctx| ctx.malloc::<u8>(len).sporulate()))
                .collect(),
            contexts,
        })
    }

    fn duplicate_cache(&self, cache: &Tensor<Self::Storage>, pos: upos) -> Tensor<Self::Storage> {
        let contexts = Arc::new(self.comms.contexts().collect::<Vec<_>>());
        InferenceConfig::duplicate_cache(
            cache,
            pos,
            |len| Cache {
                mem: contexts
                    .iter()
                    .map(|context| context.apply(|ctx| ctx.malloc::<u8>(len).sporulate()))
                    .collect(),
                contexts: contexts.clone(),
            },
            |mut dst, src| {
                for (i, context) in contexts.iter().enumerate() {
                    context.apply(|ctx| {
                        self.kernels.reform(
                            &mut dst
                                .as_mut()
                                .map_physical(|u| &mut **u.mem[i].sprout_mut(ctx)),
                            &src.as_ref().map_physical(|u| &**u.mem[i].sprout_ref(ctx)),
                            &ctx.stream(),
                        );
                    });
                }
            },
        )
    }

    fn token_embed(&self, queries: impl IntoIterator<Item = utok>) -> Tensor<Self::Storage> {
        let tokens = queries.into_iter().collect::<Vec<_>>();
        let nt = tokens.len() as udim;

        let contexts = Arc::new(self.comms.contexts().collect::<Vec<_>>());
        let dt = self.config.dt;
        let d = self.config.d;

        let mut x = Tensor::alloc(dt, &[nt, d], |len| malloc_all(&contexts, len));
        contexts[0].apply(|ctx| {
            let mut x = x.as_mut().map_physical(|u| &mut **u[0].sprout_mut(ctx));
            self.kernels.gather(
                &mut x,
                &self.embed_tokens.as_ref().map_physical(|u| &***u),
                tokens,
                self.streams[0].sprout_ref(ctx),
            );
        });
        for (i, comm) in self.comms.call().iter().enumerate() {
            contexts[i].apply(|ctx| {
                let stream = self.streams[i].sprout_ref(ctx);
                let dst = x.physical_mut()[i].sprout_mut(ctx);
                comm.broadcast(dst, None, 0, stream);
            });
        }
        x.map_physical(|mem| Cache { contexts, mem })
    }

    fn forward<'a>(
        &self,
        queries: impl IntoIterator<Item = QueryContext<'a, Self::Storage>>,
        mut token_embedded: Tensor<Self::Storage>,
    ) -> Tensor<Self::Storage>
    where
        Self: 'a,
    {
        let queries = queries.into_iter().collect::<Vec<_>>();
        let mut nt = 0;
        let mut max_seq_len = 0;
        let mut max_att_len = 0;
        let seq_len = queries
            .iter()
            .map(|q| {
                let seq = q.seq_len();
                let att = q.att_len();
                nt += seq;
                max_seq_len = max_seq_len.max(seq);
                max_att_len = max_att_len.max(att);
                seq
            })
            .collect::<Vec<_>>();
        let seq_len = &seq_len;

        let dt = self.config.dt;
        let d = self.config.d;
        let nh = self.config.nh;
        let nkvh = self.config.nkvh;
        let dh = d / nh;
        let dkv = nkvh * dh;
        let di = self.config.di;

        let n = self.comms.len() as udim;
        let reusing = (d + dkv + dkv).max(di + di);
        let pos = causal_lm::pos(&queries, nt);
        let pos = &pos;

        let x = token_embedded
            .as_mut()
            .map_physical(|u| unsafe { u.split() });
        let queries = queries
            .into_iter()
            .map(|q| {
                (
                    q.cache.map(|t| {
                        let ptrs = unsafe { t.physical_mut().split() };
                        Tensor::new(t.data_layout(), t.shape(), ptrs)
                    }),
                    q.range,
                )
            })
            .collect::<Vec<_>>();
        let queries = &queries;

        std::thread::scope(|s| {
            let _ = self
                .comms
                .iter()
                .enumerate()
                .map(|(i, comm)| {
                    let mut x = x.as_ref().map_physical(|u| unsafe {
                        std::slice::from_raw_parts_mut(u[i].0 as *mut DevByte, u[i].1)
                    });
                    let pos = pos.as_ref().map_physical(|u| &**u);
                    let mut queries = queries
                        .iter()
                        .map(|(cache, range)| {
                            (
                                cache.as_ref().map(|t| {
                                    t.as_ref().map_physical(|u| unsafe {
                                        std::slice::from_raw_parts_mut(
                                            u[i].0 as *mut DevByte,
                                            u[i].1,
                                        )
                                    })
                                }),
                                range,
                            )
                        })
                        .collect::<Vec<_>>();

                    s.spawn(move || {
                        comm.device().retain_primary().apply(|ctx| {
                            let mut queries = queries
                                .iter_mut()
                                .map(|(cache, range)| QueryContext {
                                    cache: cache.as_mut(),
                                    range: range.clone(),
                                })
                                .collect::<Vec<_>>();

                            let stream = self.streams[i].sprout_ref(ctx);

                            let pos = pos.map_physical(|u| stream.from_host(u));
                            let mut state_buf = Tensor::alloc(dt, &[nt, d + reusing / n], |len| {
                                stream.malloc::<u8>(len)
                            });
                            let buf_len_common = (nh / n * max_seq_len) as usize * dt.nbytes();
                            let mut q_buf = stream.malloc::<u8>(buf_len_common * dh as usize);
                            let mut att_buf =
                                stream.malloc::<u8>(buf_len_common * max_att_len as usize);

                            for layer in 0..self.config.nlayers as usize {
                                let params = self.matrix.get(layer, i, ctx);

                                self.self_att(
                                    &self.kernels,
                                    &mut queries,
                                    seq_len,
                                    &params,
                                    &mut x,
                                    &mut state_buf,
                                    &pos,
                                    &mut q_buf,
                                    &mut att_buf,
                                    i,
                                    layer,
                                    nt,
                                    stream,
                                );
                                comm.all_reduce(
                                    x.physical_mut(),
                                    None,
                                    self.config.dt,
                                    ReduceType::ncclSum,
                                    stream,
                                );

                                self.mlp(&self.kernels, &params, &mut x, &mut state_buf, i, stream);
                                comm.all_reduce(
                                    x.physical_mut(),
                                    None,
                                    self.config.dt,
                                    ReduceType::ncclSum,
                                    stream,
                                );
                            }

                            pos.take_physical().drop_on(stream);
                            att_buf.drop_on(stream);
                            q_buf.drop_on(stream);
                            state_buf.take_physical().drop_on(stream);
                        })
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|t| t.join().unwrap())
                .collect::<Vec<_>>();
        });
        token_embedded
    }

    fn decode(
        &self,
        decoding: impl IntoIterator<Item = DecodingMeta>,
        mut hidden_state: Tensor<Self::Storage>,
    ) -> Tensor<Self::Storage> {
        let dt = self.config.dt;
        let d = self.config.d;

        let contexts = Arc::new(vec![self.comms.contexts().next().unwrap()]);
        let ans = contexts[0].apply(|ctx| {
            let stream = self.streams[0].sprout_ref(ctx);

            let mut x = hidden_state
                .as_mut()
                .map_physical(|u| &mut **u.mem[0].sprout_mut(ctx));
            let range = DecodingMeta::select(&mut x, decoding, |dst, src| {
                stream.memcpy_d2d(dst, src);
            });

            if range.is_empty() {
                return Tensor::alloc(dt, &[0, d as _], |_| Cache {
                    contexts: contexts.clone(),
                    mem: vec![stream.malloc::<u8>(0).sporulate()],
                });
            }

            let model_norm = self
                .lm_layernorm
                .as_ref()
                .map_physical(|u| &**u.sprout_ref(ctx));
            let lm_head = self.lm_head.as_ref().map_physical(|u| &**u.sprout_ref(ctx));

            let mut x = x.slice(&[slice![range.start => range.end], slice![=>]]);
            let mut logits = Tensor::alloc(dt, &[x.shape()[0], lm_head.shape()[1]], |len| {
                stream.malloc::<u8>(len)
            });

            // 复制一个 x 以实现原地归一化
            let x_ = x
                .as_ref()
                .map_physical(|u| unsafe { from_raw_parts(u.as_ptr(), u.len()) });
            self.kernels
                .rms_norm(&mut x, &x_, &model_norm, self.config.epsilon, stream);
            self.kernels
                .mat_mul(&mut logits, 0., &x, &lm_head, 1., stream);

            logits.map_physical(|u| Cache {
                contexts: contexts.clone(),
                mem: vec![u.sporulate()],
            })
        });

        take(&mut ManuallyDrop::new(hidden_state.take_physical()).mem)
            .into_iter()
            .zip(self.comms.contexts())
            .enumerate()
            .for_each(|(i, (mem, context))| {
                context.apply(|ctx| mem.sprout(ctx).drop_on(self.streams[i].sprout_ref(ctx)));
            });

        ans
    }

    fn sample(
        &self,
        args: impl IntoIterator<Item = SampleMeta>,
        logits: Tensor<Self::Storage>,
    ) -> Vec<utok> {
        let workspace_ptr = unsafe { self.sample_workspace.as_raw() };
        let workspace_len = self.sample_workspace.len();
        let Cache { contexts, mem } = logits.physical();
        contexts[0].apply(|ctx| {
            let workspace =
                unsafe { from_raw_parts_mut(workspace_ptr as *mut DevByte, workspace_len) };
            self.kernels.sample(
                self.config.voc as _,
                args.into_iter()
                    .flat_map(|meta| repeat(meta.args).take(meta.num_decode)),
                mem[0].sprout_ref(ctx),
                workspace,
                self.streams[0].sprout_ref(ctx),
            )
        })
    }
}

impl Transformer {
    fn self_att(
        &self,
        kernels: &NvidiaKernels,
        queries: &mut [QueryContext<&mut [DevByte]>],
        seq_len: &[udim],
        params: &Layer,
        x: &mut Tensor<&mut [DevByte]>,
        state_buf: &mut Tensor<DevMem>,
        pos: &Tensor<DevMem>,
        q_buf: &mut DevMem,
        att_buf: &mut DevMem,
        i: usize,
        layer: usize,
        nt: udim,
        stream: &Stream,
    ) {
        let dt = self.config.dt;
        let d = self.config.d;
        let nh = self.config.nh;
        let nkvh = self.config.nkvh;
        let dh = d / nh;
        let dkv = nkvh * dh;
        let di = self.config.di;
        let head_group = nh / nkvh;
        let head_div = (dh as f32).sqrt().recip();
        let theta = self.config.theta;
        let epsilon = self.config.epsilon;

        let n = self.comms.len() as udim;
        let reusing = (d + dkv + dkv).max(di + di);

        let (mut x1, qkv) = split!(state_buf.as_mut().map_physical(|u| LocalSplitable::from(&mut **u)); [1]: d, reusing / n);
        let mut qkv = qkv.slice(&[slice![=>], slice![=> (d + dkv + dkv) / n]]);

        kernels.rms_norm(&mut x1, x, &params.input_layernorm(), epsilon, stream);
        kernels.mat_mul(&mut qkv, 0., &x1, &params.w_qkv(), 1., stream);

        let (q, k, v) = split!(qkv; [1]: d / n, dkv / n, dkv / n);
        let mut q = q.reshape(&[nt, nh / n, dh]);
        let mut k = k.reshape(&[nt, nkvh / n, dh]);
        let v = v.reshape(&[nt, nkvh / n, dh]);
        let o = x1.reshape(&[nt, nh, dh]);
        let o = o.slice(&[slice![=>], slice![=> nh / n], slice![=>]]);

        kernels.rope(&mut q, pos, theta, stream);
        kernels.rope(&mut k, pos, theta, stream);

        let q = q.transpose(&[1, 0, 2]).split(1, seq_len);
        let k = k.transpose(&[1, 0, 2]).split(1, seq_len);
        let v = v.transpose(&[1, 0, 2]).split(1, seq_len);
        let o = o.transpose(&[1, 0, 2]).split(1, seq_len);

        for (query, q, k, v, mut o) in izip!(queries, q, k, v, o) {
            let pos = query.pos();
            let seq_len = query.seq_len();
            let att_len = query.att_len();
            let Some((mut k_cache, mut v_cache)) = query.cache(layer as _) else {
                continue;
            };

            let slice_cat = &[slice![=>], slice![pos =>=> seq_len], slice![=>]];
            let slice_att = &[slice![=>], slice![      => att_len], slice![=>]];
            let shape_q0 = &[nkvh / n * head_group, seq_len, dh];
            let shape_q1 = &[nkvh / n, head_group * seq_len, dh];
            let shape_att0 = &[nkvh / n, head_group * seq_len, att_len];
            let shape_att1 = &[nkvh / n * head_group, seq_len, att_len];

            let mut q_att = Tensor::new(dt, shape_q0, &mut q_buf[..]);
            let mut k_cat = k_cache.as_mut().slice(slice_cat).map_physical(|u| &mut **u);
            let mut v_cat = v_cache.as_mut().slice(slice_cat).map_physical(|u| &mut **u);
            kernels.reform(&mut q_att, &q, stream);
            kernels.reform(&mut k_cat, &k, stream);
            kernels.reform(&mut v_cat, &v, stream);

            let q_att = q_att.reshape(shape_q1);
            let k_att = k_cache.slice(slice_att).transpose(&[0, 2, 1]);
            let v_att = v_cache.slice(slice_att);

            let mut att = Tensor::new(dt, shape_att0, &mut att_buf[..]);
            kernels.mat_mul(&mut att, 0., &q_att, &k_att, head_div, stream);
            let mut att = att.reshape(shape_att1);
            kernels.softmax(&mut att, stream);
            let mut x2 = q_att;
            kernels.mat_mul(&mut x2, 0., &att.reshape(shape_att0), &v_att, 1., stream);

            kernels.reform(&mut o, &x2.reshape(shape_q0), stream);
        }

        let (x1, _) = split!(state_buf.as_mut().map_physical(|u| LocalSplitable::from(&mut **u)); [1]: d, reusing / n);

        let o = x1.as_ref().slice(&[slice![=>], slice![=> d/n as udim]]);
        let o = o.map_physical(|u| &**u);
        kernels.mat_mul(
            x,
            if i == 0 { 1. } else { 0. },
            &o,
            &params.w_o(),
            1.,
            stream,
        );
    }

    fn mlp(
        &self,
        kernels: &NvidiaKernels,
        params: &Layer,
        x: &mut Tensor<&mut [DevByte]>,
        state_buf: &mut Tensor<DevMem>,
        i: usize,
        stream: &Stream,
    ) {
        let d = self.config.d;
        let nh = self.config.nh;
        let nkvh = self.config.nkvh;
        let dh = d / nh;
        let dkv = nkvh * dh;
        let di = self.config.di;
        let epsilon = self.config.epsilon;

        let n = self.comms.len() as udim;
        let reusing = (d + dkv + dkv).max(di + di);

        let (mut x1, gate_up) = split!(state_buf.as_mut().map_physical(|u| LocalSplitable::from(&mut **u)); [1]: d, reusing / n);
        let mut gate_up = gate_up.slice(&[slice![=>], slice![=> (di + di) / n]]);

        kernels.rms_norm(&mut x1, x, &params.post_att_layernorm(), epsilon, stream);
        kernels.mlp(
            x,
            &x1,
            &mut gate_up,
            &params.mlp_gate_up(),
            &params.mlp_down(),
            1.,
            i == 0,
            stream,
        );
    }
}

impl Drop for Transformer {
    #[inline]
    fn drop(&mut self) {
        let contexts = self.comms.contexts().collect::<Vec<_>>();
        unsafe {
            contexts[0].apply(|ctx| {
                ManuallyDrop::take(self.embed_tokens.physical_mut()).sprout(ctx);
                ManuallyDrop::take(self.lm_layernorm.physical_mut()).sprout(ctx);
                ManuallyDrop::take(self.lm_head.physical_mut()).sprout(ctx);
            });
            self.matrix.kill(&contexts);
            for (context, stream) in zip(contexts, std::mem::take(&mut self.streams)) {
                context.apply(|ctx| drop(stream.sprout(ctx)));
            }
        }
    }
}

pub struct Cache {
    pub contexts: Arc<Vec<Context>>,
    pub mem: Vec<DevMemSpore>,
}

impl Cache {
    unsafe fn split(&mut self) -> Vec<(cuda::bindings::CUdeviceptr, usize)> {
        self.mem
            .iter()
            .map(|mem| (mem.as_raw(), mem.len()))
            .collect()
    }
}

impl Drop for Cache {
    #[inline]
    fn drop(&mut self) {
        let mem = std::mem::take(&mut self.mem);
        for (context, mem) in zip(&*self.contexts, mem) {
            context.apply(|ctx| drop(mem.sprout(ctx)));
        }
    }
}

fn malloc_all(contexts: &[Context], len: usize) -> Vec<DevMemSpore> {
    contexts
        .iter()
        .map(|context| context.apply(|ctx| ctx.malloc::<u8>(len).sporulate()))
        .collect()
}

#[test]
fn test_infer() {
    if let Err(cuda::NoDevice) = cuda::init() {
        return;
    }
    if cuda::Device::count() >= 2 {
        causal_lm::test_impl::<Transformer>(
            [0, 1].map(cuda::Device::new).into_iter().collect(),
            &[
                29966, 29989, 1792, 29989, 29958, 13, 29903, 388, 376, 18567, 29908, 304, 592,
                21106, 29879, 5299, 29989, 465, 22137, 29989, 29958, 13,
            ],
        );
    }
}
