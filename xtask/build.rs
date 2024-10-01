﻿fn main() {
    use build_script_cfg::Cfg;
    use search_ascend_tools::find_ascend_toolkit_home;
    use search_cuda_tools::{find_cuda_root, find_nccl_root};
    use search_neuware_tools::find_neuware_home;

    let cuda = Cfg::new("detected_cuda");
    let nccl = Cfg::new("detected_nccl");
    if cfg!(feature = "nvidia") && find_cuda_root().is_some() {
        cuda.define();
        if find_nccl_root().is_some() {
            nccl.define();
        }
    }

    let neuware = Cfg::new("detected_neuware");
    if cfg!(feature = "cambricon") && find_neuware_home().is_some() {
        neuware.define();
    }

    let ascend = Cfg::new("detected_ascend");
    if cfg!(feature = "ascend") && find_ascend_toolkit_home().is_some() {
        ascend.define();
    }
}
