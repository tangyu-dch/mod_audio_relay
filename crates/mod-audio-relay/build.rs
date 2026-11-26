fn main() {
    println!("cargo:rerun-if-env-changed=SPEEXDSP_LIB_DIR");
    println!("cargo:rerun-if-env-changed=SPEEXDSP_NO_PKG_CONFIG");

    // FreeSWITCH 符号（switch_*、SWITCH_*）由宿主进程在 dlopen 时提供。
    // macOS 的 ld 默认拒绝未定义符号；dynamic_lookup 是可加载模块的标准
    // 处理方式，在其他平台上无副作用。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-undefined,dynamic_lookup");
    }

    // libspeexdsp 提供 speex_resampler_* 入口，与宿主进程中
    // FreeSWITCH 使用的是同一个库。pkg-config 覆盖大多数发行版安装；
    // SPEEXDSP_LIB_DIR 覆盖自定义前缀，SPEEXDSP_NO_PKG_CONFIG=1 强制
    // 使用裸 -lspeexdsp 回退。
    if std::env::var_os("SPEEXDSP_NO_PKG_CONFIG").is_none()
        && pkg_config::Config::new().probe("speexdsp").is_ok()
    {
        return;
    }
    if let Ok(dir) = std::env::var("SPEEXDSP_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
    }
    println!("cargo:rustc-link-lib=speexdsp");
}
