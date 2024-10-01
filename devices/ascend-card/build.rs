fn main() {
    use build_script_cfg::Cfg;
    use search_ascend_tools::find_ascend_toolkit_home;

    let ascend = Cfg::new("detected_ascend");
    if find_ascend_toolkit_home().is_some() {
        ascend.define();
    }
}
