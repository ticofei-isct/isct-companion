//! File protocol shared by the desktop shell and its user-level menu installer.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const HELPER: &str = "isct-campus-menu-setup.exe";
pub const VERB_KEYS: [&str; 6] = [
    r"Software\Classes\SystemFileAssociations\.pdf\shell\IsctCompanionCampusPrint",
    r"Software\Classes\SystemFileAssociations\.jpg\shell\IsctCompanionCampusPrint",
    r"Software\Classes\SystemFileAssociations\.jpeg\shell\IsctCompanionCampusPrint",
    r"Software\Classes\SystemFileAssociations\.png\shell\IsctCompanionCampusPrint",
    r"Software\Classes\SystemFileAssociations\.bmp\shell\IsctCompanionCampusPrint",
    r"Software\Classes\SystemFileAssociations\.webp\shell\IsctCompanionCampusPrint",
];
pub const PRINT_FLAG: &str = "--campus-print";

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub executable: PathBuf,
    pub label: String,
}

#[derive(Serialize, Deserialize)]
pub enum Outcome {
    Added,
    Failed(String),
}
