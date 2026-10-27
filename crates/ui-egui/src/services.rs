/// Host services (file dialogs) injected by the app; absent in tests and on the web.
#[derive(Default)]
pub struct Services {
    pub pick_open: Option<Box<dyn Fn(&str, &[&str]) -> Option<String>>>,
    /// Like `pick_open`, choosing several files at once (empty when cancelled).
    pub pick_open_many: Option<Box<dyn Fn(&str, &[&str]) -> Vec<String>>>,
    pub pick_save: Option<Box<dyn Fn(&str, &str) -> Option<String>>>,
    pub pick_folder: Option<Box<dyn Fn(&str) -> Option<String>>>,
}
