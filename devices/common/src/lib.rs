use common::utok;
use operators::{fuesd_softmax, mat_mul, mlp, reform, rms_norm, rope, Handle, Operator, QueueOf};
use std::ops::{Deref, DerefMut};
use tensor::Tensor;

pub type SliceOn<H> = [<H as Handle>::Byte];

pub trait Operators {
    type Handle: Handle;

    fn reform_op(&self, queue: &QueueOf<Self::Handle>) -> &impl reform::Reform<Self::Handle>;
    fn rms_norm_op(&self, queue: &QueueOf<Self::Handle>) -> &impl rms_norm::RmsNorm<Self::Handle>;
    fn mat_mul_op(&self, queue: &QueueOf<Self::Handle>) -> &impl mat_mul::MatMul<Self::Handle>;
    fn rope_op(&self, queue: &QueueOf<Self::Handle>) -> &impl rope::Rope<Self::Handle>;
    fn softmax_op(
        &self,
        queue: &QueueOf<Self::Handle>,
    ) -> &impl fuesd_softmax::FusedSoftmax<Self::Handle>;
    fn mlp_op(&self, queue: &QueueOf<Self::Handle>) -> &impl mlp::Mlp<Self::Handle>;
}

pub trait KernelsA {
    type Handle: Handle;

    fn reform<T, U>(&self, dst: &mut Tensor<T>, src: &Tensor<U>, queue: &QueueOf<Self::Handle>)
    where
        T: DerefMut<Target = SliceOn<Self::Handle>>,
        U: Deref<Target = SliceOn<Self::Handle>>;

    fn rms_norm<T, U, V>(
        &self,
        y: &mut Tensor<T>,
        x: &Tensor<U>,
        w: &Tensor<V>,
        epsilon: f32,
        queue: &QueueOf<Self::Handle>,
    ) where
        T: DerefMut<Target = SliceOn<Self::Handle>>,
        U: Deref<Target = SliceOn<Self::Handle>>,
        V: Deref<Target = SliceOn<Self::Handle>>;

    fn rope<T, U>(
        &self,
        t: &mut Tensor<T>,
        pos: &Tensor<U>,
        theta: f32,
        queue: &QueueOf<Self::Handle>,
    ) where
        T: DerefMut<Target = SliceOn<Self::Handle>>,
        U: Deref<Target = SliceOn<Self::Handle>>;

    fn mat_mul<T, U, V>(
        &self,
        c: &mut Tensor<T>,
        beta: f32,
        a: &Tensor<U>,
        b: &Tensor<V>,
        alpha: f32,
        queue: &QueueOf<Self::Handle>,
    ) where
        T: DerefMut<Target = SliceOn<Self::Handle>>,
        U: Deref<Target = SliceOn<Self::Handle>>,
        V: Deref<Target = SliceOn<Self::Handle>>;

    fn softmax<T>(&self, att: &mut Tensor<T>, queue: &QueueOf<Self::Handle>)
    where
        T: DerefMut<Target = SliceOn<Self::Handle>>;

    #[allow(clippy::too_many_arguments)]
    fn mlp<M0, M1, C0, C1, C2>(
        &self,
        x: &mut Tensor<M0>,
        x1: &Tensor<C0>,
        gate_up: &mut Tensor<M1>,
        w_gate_up: &Tensor<C1>,
        w_down: &Tensor<C2>,
        down_alpha: f32,
        down_bias: bool,
        queue: &QueueOf<Self::Handle>,
    ) where
        M0: DerefMut<Target = SliceOn<Self::Handle>>,
        M1: DerefMut<Target = SliceOn<Self::Handle>>,
        C0: Deref<Target = SliceOn<Self::Handle>>,
        C1: Deref<Target = SliceOn<Self::Handle>>,
        C2: Deref<Target = SliceOn<Self::Handle>>;
}

pub trait KernelsB {
    type Handle: Handle;

    fn gather<T, U, I>(
        &self,
        x: &mut Tensor<T>,
        table: &Tensor<U>,
        tokens: I,
        queue: &QueueOf<Self::Handle>,
    ) where
        T: DerefMut<Target = SliceOn<Self::Handle>>,
        U: Deref<Target = [u8]>,
        I: IntoIterator<Item = utok>;
}

pub trait Kernels<H: Handle>: KernelsA<Handle = H> + KernelsB<Handle = H> {}

impl<Ops: Operators> KernelsA for Ops {
    type Handle = <Ops as Operators>::Handle;

    fn reform<T, U>(&self, dst: &mut Tensor<T>, src: &Tensor<U>, queue: &QueueOf<Self::Handle>)
    where
        T: DerefMut<Target = SliceOn<Self::Handle>>,
        U: Deref<Target = SliceOn<Self::Handle>>,
    {
        if let Err(e) = self.reform_op(queue).launch(
            &reform::Args {
                dst_layout: dst.layout(),
                dst_base: dst.base_mut(),
                src_layout: src.layout(),
                src_base: src.base(),
            },
            queue,
        ) {
            panic!(
                "\
reform failed: {e}
{:?}|{:?}->{:?}",
                src.shape(),
                src.strides(),
                dst.strides(),
            );
        }
    }

    fn rms_norm<T, U, V>(
        &self,
        y: &mut Tensor<T>,
        x: &Tensor<U>,
        w: &Tensor<V>,
        epsilon: f32,
        queue: &QueueOf<Self::Handle>,
    ) where
        T: DerefMut<Target = SliceOn<Self::Handle>>,
        U: Deref<Target = SliceOn<Self::Handle>>,
        V: Deref<Target = SliceOn<Self::Handle>>,
    {
        self.rms_norm_op(queue)
            .launch(
                &rms_norm::Args {
                    y_layout: y.layout(),
                    y_base: y.base_mut(),
                    x_layout: x.layout(),
                    x_base: x.base(),
                    w_layout: w.layout(),
                    w_base: w.base(),
                    epsilon,
                },
                queue,
            )
            .unwrap();
    }

    fn rope<T, U>(
        &self,
        t: &mut Tensor<T>,
        pos: &Tensor<U>,
        theta: f32,
        queue: &QueueOf<Self::Handle>,
    ) where
        T: DerefMut<Target = SliceOn<Self::Handle>>,
        U: Deref<Target = SliceOn<Self::Handle>>,
    {
        self.rope_op(queue)
            .launch(
                &rope::Args {
                    t_layout: t.layout(),
                    t_base: t.base_mut(),
                    p_layout: pos.layout(),
                    p_base: pos.base(),
                    theta,
                },
                queue,
            )
            .unwrap();
    }

    fn mat_mul<T, U, V>(
        &self,
        c: &mut Tensor<T>,
        beta: f32,
        a: &Tensor<U>,
        b: &Tensor<V>,
        alpha: f32,
        queue: &QueueOf<Self::Handle>,
    ) where
        T: DerefMut<Target = SliceOn<Self::Handle>>,
        U: Deref<Target = SliceOn<Self::Handle>>,
        V: Deref<Target = SliceOn<Self::Handle>>,
    {
        self.mat_mul_op(queue)
            .launch(
                &mat_mul::Args {
                    c_layout: c.layout(),
                    c_base: c.base_mut(),
                    beta,
                    a_layout: a.layout(),
                    a_base: a.base(),
                    b_layout: b.layout(),
                    b_base: b.base(),
                    alpha,
                },
                queue,
            )
            .unwrap();
    }

    fn softmax<T>(&self, att: &mut Tensor<T>, queue: &QueueOf<Self::Handle>)
    where
        T: DerefMut<Target = SliceOn<Self::Handle>>,
    {
        self.softmax_op(queue)
            .launch(
                &fuesd_softmax::Args {
                    att_layout: att.layout(),
                    att_base: att.base_mut(),
                },
                queue,
            )
            .unwrap();
    }

    fn mlp<M0, M1, C0, C1, C2>(
        &self,
        x: &mut Tensor<M0>,
        x1: &Tensor<C0>,
        gate_up: &mut Tensor<M1>,
        w_gate_up: &Tensor<C1>,
        w_down: &Tensor<C2>,
        down_alpha: f32,
        down_bias: bool,
        queue: &QueueOf<Self::Handle>,
    ) where
        M0: DerefMut<Target = SliceOn<Self::Handle>>,
        M1: DerefMut<Target = SliceOn<Self::Handle>>,
        C0: Deref<Target = SliceOn<Self::Handle>>,
        C1: Deref<Target = SliceOn<Self::Handle>>,
        C2: Deref<Target = SliceOn<Self::Handle>>,
    {
        self.mlp_op(queue)
            .launch(
                &mlp::Args {
                    y_layout: x.layout(),
                    y_base: x.base_mut(),
                    x_layout: x1.layout(),
                    x_base: x1.base(),
                    gate_up_layout: gate_up.layout(),
                    gate_up_base: gate_up.base_mut(),
                    w_gate_up_layout: w_gate_up.layout(),
                    w_gate_up_base: w_gate_up.base(),
                    w_down_layout: w_down.layout(),
                    w_down_base: w_down.base(),
                    down_alpha,
                    down_bias,
                },
                queue,
            )
            .unwrap();
    }
}
