pub fn list_turbo() {
    #[cfg(detected_cuda)]
    list_nv();
    #[cfg(detected_neuware)]
    list_cn();
    #[cfg(detected_ascend)]
    list_acl();
}

#[cfg(detected_cuda)]
fn list_nv() {
    use llama_nv::cuda::{self, Device as Gpu};

    if let Err(cuda::NoDevice) = cuda::init() {
        return;
    }
    println!("NVidia CUDA environment detected, use `--turbo nv` to select.");
    for i in 0..Gpu::count() {
        let gpu = Gpu::new(i as _);
        println!(
            "GPU{i}: {} | cc={} | memory={}",
            gpu.name(),
            gpu.compute_capability(),
            gpu.total_memory(),
        );
    }
    println!();
}

#[cfg(detected_neuware)]
fn list_cn() {
    use llama_cn::cndrv::{self, Device as Mlu};

    cndrv::init();
    println!("Cambricon Neuware environment detected, use `--turbo cn` to select.");
    for i in 0..Mlu::count() {
        let mlu = Mlu::new(i as _);
        println!(
            "MLU{i}: {} | isa={} | memory={}",
            mlu.name(),
            mlu.isa(),
            mlu.total_memory(),
        );
    }
    println!();
}

#[cfg(detected_ascend)]
fn list_acl() {
    use common_acl::ascendcl::{self, Device as Card};

    ascendcl::init();
    println!("AscendCL environment detected, use `--turbo acl` to select.");
    for i in 0..Card::count() {
        let card = Card::new(i as _);
        println!("Ascend{i}: {}", card.name().to_str().unwrap());
    }
    println!();
}
