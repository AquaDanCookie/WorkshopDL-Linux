mod app;
mod desktop;
mod games;
mod ggntw;
mod jobs;
mod settings;
mod steamcmd;
mod util;
mod workshop_api;

use std::error::Error;

fn main() -> eframe::Result<()> {
    let project = util::project_dir();
    let settings = settings::Settings::load(&project);
    desktop::install_integration(&project);

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title(desktop::APP_NAME)
        .with_app_id(desktop::APP_ID)
        .with_inner_size([settings.window_width as f32, settings.window_height as f32])
        .with_min_inner_size([880.0, 600.0]);
    if let Some(icon) = desktop::icon_data() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        desktop::APP_NAME,
        options,
        Box::new(
            move |cc: &eframe::CreationContext<'_>| -> Result<
                Box<dyn eframe::App>,
                Box<dyn Error + Send + Sync>,
            > { Ok(Box::new(app::WorkshopApp::new(cc, project, settings))) },
        ),
    )
}
