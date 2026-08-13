//! Native macOS menu bar (muda). Replaces the deleted in-app
//! File/Edit/View/Window row (§4.1 single chrome). Every command routes back
//! through the same app handlers the in-app menus used, so the unsaved-changes
//! guard and project lifecycle semantics are unchanged. Quit is a custom item
//! (not the `terminate:` predefined) so the guard can intercept it.
#![cfg(target_os = "macos")]
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{AboutMetadata, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use std::path::PathBuf;

const IMPORT_STL_LABEL: &str = "Import STL / STEP Geometry…";
const RUN_ANALYSIS_LABEL: &str = "Run Analysis";
const EXPORT_FEA_LABEL: &str = "Export Fluid Surface Loads for FEA…";
const NO_RECENT_PROJECTS_LABEL: &str = "No Recent Projects";
const START_AUTO_ADVANCE_LABEL: &str = "Start Sandbox Auto-Advance";
const STOP_AUTO_ADVANCE_LABEL: &str = "Stop Sandbox Auto-Advance";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuCommand {
    NewProject,
    OpenProject,
    SaveProject,
    SaveProjectAs,
    ImportModel,
    ImportCad,
    ExportCalculations,
    Quit,
    UndoCaseEdit,
    RedoCaseEdit,
    ResetControls,
    ResetCamera,
    ToggleDimension,
    RunAnalysis,
    ExportFea,
    RegenerateSandbox,
    ToggleSandboxLive,
    OpenDocs,
    CheckForUpdates,
    SignOut,
}

pub enum MenuSignal {
    Command(MenuCommand),
    OpenRecent(PathBuf),
}

/// State the menu bar mirrors; items are only touched when this changes.
#[derive(Clone, PartialEq, Default)]
pub struct MenuSyncState {
    pub can_save: bool,
    pub can_undo_case_edit: bool,
    pub can_redo_case_edit: bool,
    pub analysis_available: bool,
    pub fea_export_available: bool,
    pub sandbox_enabled: bool,
    pub sandbox_live: bool,
    pub recents: Vec<(String, PathBuf)>,
}

pub struct MenuBar {
    _menu: Menu,
    commands: Vec<(MenuId, MenuCommand)>,
    save_item: MenuItem,
    undo_case_edit_item: MenuItem,
    redo_case_edit_item: MenuItem,
    run_item: MenuItem,
    export_fea_item: MenuItem,
    research_menu: Submenu,
    live_item: MenuItem,
    recent_menu: Submenu,
    empty_recent_item: MenuItem,
    recent_items: Vec<(MenuId, PathBuf, MenuItem)>,
    synced: MenuSyncState,
}

fn cmd(key: Code) -> Option<Accelerator> {
    Some(Accelerator::new(Some(Modifiers::META), key))
}

fn cmd_shift(key: Code) -> Option<Accelerator> {
    Some(Accelerator::new(
        Some(Modifiers::META | Modifiers::SHIFT),
        key,
    ))
}

impl MenuBar {
    /// Build and install the menu bar on the running NSApp. Returns `None`
    /// if any menu operation fails, in which case the caller falls back to
    /// keyboard shortcuts only (project actions all remain reachable
    /// in-app; this never hides functionality behind the menu alone).
    pub fn install() -> Option<Self> {
        let menu = Menu::new();
        let mut commands = Vec::new();
        let mut item = |text: &str, accel: Option<Accelerator>, command: MenuCommand| {
            let entry = MenuItem::new(text, true, accel);
            commands.push((entry.id().clone(), command));
            entry
        };

        let quit_item = item("Quit Reyn Studio", cmd(Code::KeyQ), MenuCommand::Quit);
        let app_menu = Submenu::new("Reyn Studio", true);
        app_menu
            .append_items(&[
                &PredefinedMenuItem::about(
                    Some("About Reyn Studio"),
                    Some(AboutMetadata {
                        name: Some("Reyn Studio".into()),
                        comments: Some(
                            "Native neural-CFD workbench — local-first, evidence-first.".into(),
                        ),
                        ..Default::default()
                    }),
                ),
                &PredefinedMenuItem::separator(),
                &item("Sign Out…", None, MenuCommand::SignOut),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::services(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::hide(None),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &PredefinedMenuItem::separator(),
                &quit_item,
            ])
            .ok()?;

        let recent_menu = Submenu::new("Open Recent", true);
        let empty_recent_item = MenuItem::new(NO_RECENT_PROJECTS_LABEL, false, None);
        recent_menu.append(&empty_recent_item).ok()?;
        let save_item = item("Save Project", cmd(Code::KeyS), MenuCommand::SaveProject);
        let file_menu = Submenu::new("File", true);
        file_menu
            .append_items(&[
                &item("New Project", cmd(Code::KeyN), MenuCommand::NewProject),
                &item("Open Project…", cmd(Code::KeyO), MenuCommand::OpenProject),
                &recent_menu,
                &PredefinedMenuItem::separator(),
                &save_item,
                &item(
                    "Save Project As…",
                    cmd_shift(Code::KeyS),
                    MenuCommand::SaveProjectAs,
                ),
                &PredefinedMenuItem::separator(),
                &item("Import Model…", None, MenuCommand::ImportModel),
                &item(IMPORT_STL_LABEL, None, MenuCommand::ImportCad),
                &item(
                    "Export Calculations…",
                    None,
                    MenuCommand::ExportCalculations,
                ),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::close_window(Some("Close Window")),
            ])
            .ok()?;

        let undo_case_edit_item = item(
            "Undo Case-Draft Edit",
            cmd(Code::KeyZ),
            MenuCommand::UndoCaseEdit,
        );
        let redo_case_edit_item = item(
            "Redo Case-Draft Edit",
            cmd_shift(Code::KeyZ),
            MenuCommand::RedoCaseEdit,
        );
        let edit_menu = Submenu::new("Edit", true);
        edit_menu
            .append_items(&[
                &undo_case_edit_item,
                &redo_case_edit_item,
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::cut(None),
                &PredefinedMenuItem::copy(None),
                &PredefinedMenuItem::paste(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::select_all(None),
                &PredefinedMenuItem::separator(),
                &item("Reset Controls", None, MenuCommand::ResetControls),
            ])
            .ok()?;

        let view_menu = Submenu::new("View", true);
        view_menu
            .append_items(&[
                &item("Reset Camera", None, MenuCommand::ResetCamera),
                &item("Toggle 2D / 3D View", None, MenuCommand::ToggleDimension),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::fullscreen(None),
            ])
            .ok()?;

        let run_item = item(
            RUN_ANALYSIS_LABEL,
            cmd(Code::KeyR),
            MenuCommand::RunAnalysis,
        );
        let export_fea_item = item(EXPORT_FEA_LABEL, None, MenuCommand::ExportFea);
        let analysis_menu = Submenu::new("Analysis", true);
        analysis_menu
            .append_items(&[&run_item, &export_fea_item])
            .ok()?;

        let live_item = item(
            START_AUTO_ADVANCE_LABEL,
            None,
            MenuCommand::ToggleSandboxLive,
        );
        let research_menu = Submenu::new("Research", true);
        research_menu
            .append_items(&[
                &item(
                    "Regenerate Sandbox Field",
                    None,
                    MenuCommand::RegenerateSandbox,
                ),
                &live_item,
            ])
            .ok()?;

        let window_menu = Submenu::new("Window", true);
        window_menu
            .append_items(&[
                &PredefinedMenuItem::minimize(None),
                &PredefinedMenuItem::maximize(Some("Zoom")),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::bring_all_to_front(None),
            ])
            .ok()?;

        let help_menu = Submenu::new("Help", true);
        help_menu
            .append_items(&[
                &item("Reyn Studio Documentation", None, MenuCommand::OpenDocs),
                &PredefinedMenuItem::separator(),
                &item("Check for Updates…", None, MenuCommand::CheckForUpdates),
            ])
            .ok()?;

        menu.append_items(&[
            &app_menu,
            &file_menu,
            &edit_menu,
            &view_menu,
            &analysis_menu,
            &research_menu,
            &window_menu,
            &help_menu,
        ])
        .ok()?;
        menu.init_for_nsapp();

        Some(Self {
            _menu: menu,
            commands,
            save_item,
            undo_case_edit_item,
            redo_case_edit_item,
            run_item,
            export_fea_item,
            research_menu,
            live_item,
            recent_menu,
            empty_recent_item,
            recent_items: Vec::new(),
            synced: MenuSyncState {
                // Force the first sync to write real state.
                can_save: true,
                can_undo_case_edit: true,
                can_redo_case_edit: true,
                analysis_available: true,
                fea_export_available: true,
                sandbox_enabled: true,
                sandbox_live: false,
                recents: Vec::new(),
            },
        })
    }

    /// Drain native menu clicks into app-level signals.
    pub fn poll(&self) -> Vec<MenuSignal> {
        let mut signals = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some((_, command)) = self.commands.iter().find(|(id, _)| *id == event.id) {
                signals.push(MenuSignal::Command(*command));
            } else if let Some((_, path, _)) =
                self.recent_items.iter().find(|(id, _, _)| *id == event.id)
            {
                signals.push(MenuSignal::OpenRecent(path.clone()));
            }
        }
        signals
    }

    /// Mirror app state onto the native items (enabled flags, live-toggle
    /// label, recent-projects submenu). No-ops when nothing changed.
    pub fn sync(&mut self, state: MenuSyncState) {
        if state == self.synced {
            return;
        }
        if state.can_save != self.synced.can_save {
            self.save_item.set_enabled(state.can_save);
        }
        if state.can_undo_case_edit != self.synced.can_undo_case_edit {
            self.undo_case_edit_item
                .set_enabled(state.can_undo_case_edit);
        }
        if state.can_redo_case_edit != self.synced.can_redo_case_edit {
            self.redo_case_edit_item
                .set_enabled(state.can_redo_case_edit);
        }
        if state.analysis_available != self.synced.analysis_available {
            self.run_item.set_enabled(state.analysis_available);
        }
        if state.fea_export_available != self.synced.fea_export_available {
            self.export_fea_item.set_enabled(state.fea_export_available);
        }
        if state.sandbox_enabled != self.synced.sandbox_enabled {
            self.research_menu.set_enabled(state.sandbox_enabled);
        }
        if state.sandbox_live != self.synced.sandbox_live {
            self.live_item.set_text(if state.sandbox_live {
                STOP_AUTO_ADVANCE_LABEL
            } else {
                START_AUTO_ADVANCE_LABEL
            });
        }
        if state.recents != self.synced.recents {
            for (_, _, entry) in self.recent_items.drain(..) {
                let _ = self.recent_menu.remove(&entry);
            }
            if state.recents.is_empty() {
                let _ = self.recent_menu.append(&self.empty_recent_item);
            } else {
                let _ = self.recent_menu.remove(&self.empty_recent_item);
                for (name, path) in &state.recents {
                    let file = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("project");
                    let entry = MenuItem::new(format!("{name}  ·  {file}"), true, None);
                    if self.recent_menu.append(&entry).is_ok() {
                        self.recent_items
                            .push((entry.id().clone(), path.clone(), entry));
                    }
                }
            }
        }
        self.synced = state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn native_accelerators_are_conventional_and_unique() {
        let standard = [
            cmd(Code::KeyN).unwrap(),
            cmd(Code::KeyO).unwrap(),
            cmd(Code::KeyS).unwrap(),
            cmd_shift(Code::KeyS).unwrap(),
            cmd(Code::KeyQ).unwrap(),
            cmd(Code::KeyZ).unwrap(),
            cmd_shift(Code::KeyZ).unwrap(),
            cmd(Code::KeyR).unwrap(),
        ];
        assert!(standard[0].matches(Modifiers::SUPER, Code::KeyN));
        assert!(standard[3].matches(Modifiers::SUPER | Modifiers::SHIFT, Code::KeyS));

        let ids: HashSet<_> = standard.iter().map(Accelerator::id).collect();
        assert_eq!(
            ids.len(),
            standard.len(),
            "native menu accelerators must not collide"
        );
    }

    #[test]
    fn action_labels_are_specific_and_scientifically_bounded() {
        assert!(IMPORT_STL_LABEL.ends_with('…'));
        assert!(!IMPORT_STL_LABEL.contains("CAD"));
        assert_eq!(RUN_ANALYSIS_LABEL, "Run Analysis");
        assert!(EXPORT_FEA_LABEL.starts_with("Export Fluid Surface Loads"));
        assert!(EXPORT_FEA_LABEL.ends_with('…'));
        assert!(START_AUTO_ADVANCE_LABEL.starts_with("Start "));
        assert!(STOP_AUTO_ADVANCE_LABEL.starts_with("Stop "));
    }

    #[test]
    fn contextual_actions_default_to_disabled() {
        let state = MenuSyncState::default();
        assert!(!state.can_save);
        assert!(!state.can_undo_case_edit);
        assert!(!state.can_redo_case_edit);
        assert!(!state.analysis_available);
        assert!(!state.fea_export_available);
        assert!(!state.sandbox_enabled);
        assert!(!state.sandbox_live);
        assert!(state.recents.is_empty());
        assert_eq!(NO_RECENT_PROJECTS_LABEL, "No Recent Projects");
    }
}
