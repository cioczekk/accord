mod app;

fn main() -> iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    app::VoiceApp::run()
}
