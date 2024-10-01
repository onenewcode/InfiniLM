mod infer;

use causal_lm::Model;
use common::{safe_tensors::SafeTensors, utok, FileLoadError};
use common_cpu::CpuKernels;
use digit_layout::DigitLayout;
use mixtral::{ConfigJson, MixtralParams};
use std::path::Path;
use tensor::udim;

pub struct MixtralCPU {
    bos_token: utok,
    eos_token: utok,
    data_type: DigitLayout,
    nlayers: udim,
    nh: udim,
    nkvh: udim,
    max_seq_len: udim,
    d: udim,
    di: udim,
    ne: udim,
    k: udim,
    epsilon: f32,
    theta: f32,
    params: MixtralParams,

    kernels: CpuKernels,
}

impl Model for MixtralCPU {
    type Error = FileLoadError;
    type Meta = ();

    fn load(model_dir: impl AsRef<Path>, _: Self::Meta) -> Result<Self, Self::Error> {
        let config = ConfigJson::load(&model_dir)?;
        Ok(Self {
            bos_token: config.bos_token_id,
            eos_token: config.eos_token_id,
            data_type: config.data_layout(),
            nlayers: config.num_hidden_layers as _,
            nh: config.num_attention_heads as _,
            nkvh: config.num_key_value_heads as _,
            max_seq_len: config.max_position_embeddings as _,
            d: config.hidden_size as _,
            di: config.intermediate_size as _,
            epsilon: config.rms_norm_eps,
            theta: config.rope_theta,
            params: MixtralParams::new(&config, SafeTensors::load_from_dir(model_dir)?),
            ne: config.num_local_experts as _,
            k: config.num_experts_per_tok as _,

            kernels: Default::default(),
        })
    }
}

#[test]
fn test_build() {
    use std::time::Instant;

    let t0 = Instant::now();
    let _transformer = MixtralCPU::load(
        "/data1/shared/hugging_face/Mixtral-8x7B-Instruct-v0.1_F16/",
        (),
    );
    let t1 = Instant::now();
    println!("build transformer {:?}", t1 - t0);
}
