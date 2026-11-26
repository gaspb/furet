//! `furet-app [folder]`: the database explorer as its own native window (GPUI), for the repo in
//! `folder` (default: here). It talks to the same `furet serve` as the CLI and the herdr panel, so
//! connections, tunnels, read-only rules and the history are the same.

mod assets;
mod engine;
mod explorer;
mod look;
mod results;

use gpui_kit::*;

fn main() {
    let folder = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let cwd = folder
        .canonicalize()
        .unwrap_or(folder)
        .to_string_lossy()
        .into_owned();
    let title = format!(
        "furet · {}",
        std::path::Path::new(&cwd)
            .file_name()
            .map_or(cwd.clone(), |n| n.to_string_lossy().into_owned())
    );
    application().with_assets(assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        assets::load_fonts(cx);
        explorer::bind_keys(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1280.), px(820.)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        open_window(options, cx, |window, cx| {
            look::follow_system(window, cx);
            cx.new(|cx| explorer::Explorer::new(cwd, window, cx))
        })
        .expect("cannot open the window");
        cx.activate(true);
    });
}
