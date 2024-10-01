#![doc = include_str!("../README.md")]
#![deny(warnings, missing_docs)]

mod decoding;
mod query_context;

use common::{upos, utok};
use digit_layout::types::U32;
use std::path::Path;
use tensor::{udim, Tensor};

pub use decoding::DecodingMeta;
pub use operators::random_sample::SampleArgs;
pub use query_context::QueryContext;

/// 从文件系统加载的模型。
pub trait Model: Sized {
    /// 用于模型加载的元数据。
    type Meta;
    /// 模型加载中可能的错误。
    type Error;
    /// 从文件系统加载模型。
    fn load(model_dir: impl AsRef<Path>, meta: Self::Meta) -> Result<Self, Self::Error>;
}

/// 因果语言模型。
///
/// 基于从文件加载得到的模型参数和权重，提供以下能力：
///
/// - 创建缓存张量（[`new_cache`](CausalLM::new_cache)）；
/// - 复制缓存张量（[`duplicate_cache`](CausalLM::duplicate_cache)）；
/// - 以及对输入序列计算词嵌入（[`token_embed`](CausalLM::token_embed)）；
/// - 对词嵌入计算前向传播（[`forward`](CausalLM::forward)）；
/// - 解码词嵌入张量得到概率密度（[`decode`](CausalLM::decode)）；
/// - 采样概率密度（[`sample`](CausalLM::sample)）；
///
/// 这种定义根据计算的形式和特性将“一轮”推理分割为多个部分，方便灵活地实现调度。
/// 为了在推理的不同阶段之间传递巨大的张量，需要 [`Storage`](CausalLM::Storage) 类型来约定中间变量的存储方式。
///
/// 模型的结构和计算隔离在这个特性以下，基于这个特性实现的推理调度服务可适用于不同结构的因果语言模型。
pub trait CausalLM: Model {
    /// 定义中间变量的存储方式。
    type Storage;
    /// 最大序列长度。
    fn max_seq_len(&self) -> upos;
    /// 模型定义的句子起始符。
    fn bos_token(&self) -> utok;
    /// 模型定义的句子结束符。
    fn eos_token(&self) -> utok;
    /// 创建一个未填充的缓存张量（`num_layers x 2 x num_kv_head x max_seq_len x head_dim`）。
    fn new_cache(&self) -> Tensor<Self::Storage>;
    /// 复制一个有效长度为 `pos` 的缓存。
    ///
    /// 有效部分：`.., .., .., ..pos, ..`
    fn duplicate_cache(&self, cache: &Tensor<Self::Storage>, pos: upos) -> Tensor<Self::Storage>;
    /// 对所有词执行词嵌入（`num_tokens x hidden_size`）。
    ///
    /// 词嵌入是上下文无关的，对于每个词独立进行，因此多个请求的查询序列可以 flatten 同时计算。
    fn token_embed(&self, queries: impl IntoIterator<Item = utok>) -> Tensor<Self::Storage>;
    /// 对词嵌入张量执行 Transformer 计算（`num_t   okens x hidden_size`）。
    ///
    /// 需要输入每个请求的上下文。
    fn forward<'a>(
        &self,
        queries: impl IntoIterator<Item = QueryContext<'a, Self::Storage>>,
        token_embedded: Tensor<Self::Storage>,
    ) -> Tensor<Self::Storage>
    where
        Self: 'a;
    /// 对词嵌入张量执行解码计算（`num_decoding_tokens` x `vocab_size`）。
    ///
    /// 每个请求可以独立指定解码 token 的数量。
    fn decode(
        &self,
        decoding: impl IntoIterator<Item = DecodingMeta>,
        hidden_state: Tensor<Self::Storage>,
    ) -> Tensor<Self::Storage>;
    /// 对 logits 进行采样。
    fn sample(
        &self,
        args: impl IntoIterator<Item = SampleMeta>,
        logits: Tensor<Self::Storage>,
    ) -> Vec<utok>;
}

/// 解码的要求。
pub struct SampleMeta {
    /// 解码的长度。
    pub num_decode: usize,
    /// 采样参数。
    pub args: SampleArgs,
}

/// 生成位置张量。
#[inline]
pub fn pos<'a, S: 'a>(
    queries: impl IntoIterator<Item = &'a QueryContext<'a, S>>,
    nt_hint: udim,
) -> Tensor<Vec<upos>> {
    let mut ans = Vec::with_capacity(nt_hint as usize);
    for query in queries {
        ans.extend(query.range.clone());
    }
    Tensor::new(U32, &[ans.len() as _], ans)
}

/// 测试模型实现。
pub fn test_impl<M>(meta: M::Meta, prompt: &[utok])
where
    M: CausalLM,
    M::Error: std::fmt::Debug,
{
    use std::time::Instant;

    let Some(model_dir) = common::test_model::find() else {
        return;
    };
    println!("model_dir: {}", model_dir.display());

    let t0 = Instant::now();
    let model = M::load(model_dir, meta).unwrap();
    let t1 = Instant::now();
    println!("load {:?}", t1 - t0);

    let mut cache = model.new_cache();

    let mut prompt = prompt.to_vec();
    let mut pos = 0;

    while prompt != [model.eos_token()] {
        let token_embedded = CausalLM::token_embed(&model, prompt.iter().copied());

        let queries = [QueryContext {
            cache: Some(&mut cache),
            range: pos..pos + prompt.len() as upos,
        }];
        let hidden_state = CausalLM::forward(&model, queries, token_embedded);

        let decoding = [DecodingMeta {
            num_query: prompt.len(),
            num_decode: 1,
        }];
        let logits = CausalLM::decode(&model, decoding, hidden_state);

        let args = [SampleMeta {
            num_decode: 1,
            args: SampleArgs::ARG_MAX,
        }];
        let tokens = CausalLM::sample(&model, args, logits);

        println!("{:?}", tokens);
        pos += prompt.len() as upos;
        prompt = tokens;
    }
}
