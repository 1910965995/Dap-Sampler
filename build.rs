// build.rs: 嵌入 Windows exe 图标
//
// 仅在 Windows 下编译资源脚本，把 icon/icon.ico 作为应用程序图标打入 exe。
// 图标在资源管理器、任务栏、窗口标题栏等处显示。

fn main() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("icon/icon.ico");
        // 设置版本信息中的文件描述（可选，显示在文件属性中）
        res.set("FileDescription", "DAP Sampler - CMSIS-DAP v2 Variable Sampler");
        if let Err(e) = res.compile() {
            panic!("Failed to compile Windows resource: {}", e);
        }
    }
}
