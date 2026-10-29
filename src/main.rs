mod app;
mod editor;
mod editor_view;
mod file_search;
mod highlight;
mod layout;
mod merge;
mod merge_view;
mod renderer;
mod shell;
mod terminal;
mod theme;
mod windows;
mod workspace;

fn main() -> anyhow::Result<()> {
    let mut cwd = std::env::current_dir()?;
    if cwd == std::path::Path::new("/")
        && let Some(home) = std::env::var_os("HOME")
    {
        cwd = home.into();
    }
    let mut restore = true;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--fresh" => restore = false,
            "--help" | "-h" => {
                println!(
                    "LeafTerm — GPU terminal with Git-aware file tree\n\nUsage: leafterm [--fresh] [directory]\n\n--fresh  Start a new layout instead of restoring saved windows\n\n⌘D / ⌘⇧D: Split right / down   ⌘N / ⌘T: New window   ⌘W: Close pane\n⌘⌥Arrows: Focus pane   ⌘⇧Enter: Zoom pane   F2: Rename window\n⌘B: Show / hide sidebar   ⌘P: Search files   ⌘E: Toggle Files / Changes   ⌘R: Refresh Git   ⌘C / ⌘V: Copy / paste"
                );
                return Ok(());
            }
            _ => {
                cwd = std::path::PathBuf::from(arg).canonicalize()?;
                restore = false;
            }
        }
    }
    anyhow::ensure!(cwd.is_dir(), "The working directory must be a folder");
    let mut builder = winit::event_loop::EventLoop::<windows::Event>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::EventLoopBuilderExtMacOS;
        builder.with_default_menu(false);
    }
    let event_loop = builder.build()?;
    let mut app = windows::Windows::new(event_loop.create_proxy(), cwd, restore);
    event_loop.run_app(&mut app)?;
    Ok(())
}
