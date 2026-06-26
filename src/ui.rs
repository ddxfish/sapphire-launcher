// View rendering — split out of main.rs (modular source, single output binary).
use crate::*;
use crate::platform::sapphire_env_python;
use iced::widget::{button, column, container, horizontal_space, pick_list, row, scrollable, text, text_editor, text_input, Column};
use iced::{color, Element, Font, Length, Padding};
use std::path::PathBuf;

impl App {
    // ── View ───────────────────────────────────────────────────────────────

    pub(crate) fn view(&self) -> Element<Message> {
        column![
            self.view_header(),
            self.view_tab_bar(),
            self.view_tab_content(),
            self.view_log_panel(),
        ]
        .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    // ── Header bar ─────────────────────────────────────────────────────────

    fn view_header(&self) -> Element<Message> {
        let placeholder = if cfg!(windows) { "C:\\Users\\You\\Sapphire" } else { "/home/you/sapphire" };
        let path_input = text_input(placeholder, &self.install_path)
            .on_input(Message::PathChanged)
            .width(Length::FillPortion(3));

        let browse_btn = button("Browse").on_press(Message::BrowsePath);

        // At-a-glance: does the chosen folder actually hold a Sapphire install?
        let p = PathBuf::from(&self.install_path);
        let (hint_txt, hint_color) = if p.join("main.py").exists() {
            ("installed", color!(0x4caf50))
        } else if p.exists() {
            ("no install here", color!(0xf9e154))
        } else {
            ("empty path", color!(0x7f849c))
        };
        let path_hint = text(hint_txt).size(11).color(hint_color);

        let branch_picker = pick_list(
            BRANCHES,
            self.selected_branch,
            Message::BranchSelected,
        )
        .placeholder("Branch...");

        let mut header = row![
            path_input,
            browse_btn,
            path_hint,
            horizontal_space(),
            branch_picker,
        ]
        .spacing(8)
        .padding(8)
        .align_y(iced::Alignment::Center);

        // Update indicator
        if let Some(n) = self.updates_available {
            if n > 0 {
                let badge = button(
                    text(format!("{} new", n)).size(12).color(color!(0x3d85c6))
                )
                .on_press(Message::TabSelected(Tab::Update))
                .style(button::text)
                .padding([2, 6]);
                header = header.push(badge);
            }
        }

        if self.sapphire_stopping {
            let stopping_btn = button(text("Stopping...").font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }))
            .style(button::secondary);

            header = header.push(stopping_btn);
        } else if self.sapphire_running {
            let open_btn = button(text("Open Browser").font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }))
            .on_press(Message::OpenBrowser)
            .style(button::primary);

            let stop_btn = button(text("Stop").font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }))
            .on_press(Message::StopSapphire)
            .style(button::danger);

            header = header.push(open_btn).push(stop_btn);
        } else {
            let launch_btn = button(text("Launch").font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }))
            .on_press(Message::Launch)
            .style(button::success);

            header = header.push(launch_btn);
        }

        container(header)
            .width(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(color!(0x181825))),
                ..Default::default()
            })
            .into()
    }

    // ── Tab bar ────────────────────────────────────────────────────────────

    fn view_tab_bar(&self) -> Element<Message> {
        let running_label = if self.sapphire_running { "Log *" } else { "Log" };
        let mut tabs = row![
            self.tab_button("Install", Tab::Install),
            self.tab_button("Update", Tab::Update),
            self.tab_button("Uninstall", Tab::Uninstall),
            self.tab_button("Troubleshoot", Tab::Troubleshoot),
            self.tab_button(running_label, Tab::Running),
        ]
        .spacing(0);

        // Autostart tab: shown when autostart is set up OR Sapphire is installed (so an
        // orphaned setup stays removable after an uninstall). Linux = systemd unit;
        // Windows = our autostart marker file (autostart.on) existing.
        let main_py = PathBuf::from(&self.install_path).join("main.py").exists();
        #[cfg(not(windows))]
        let show_autostart = self.service.is_some() || main_py;
        #[cfg(windows)]
        let show_autostart = main_py || crate::platform::autostart_marker_path().exists();
        if show_autostart {
            tabs = tabs.push(self.tab_button("Autostart", Tab::Service));
        }

        container(tabs)
            .width(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(color!(0x11111b))),
                ..Default::default()
            })
            .into()
    }

    fn tab_button<'a>(&self, label: &'a str, tab: Tab) -> Element<'a, Message> {
        let is_active = self.active_tab == tab;

        let btn = button(text(label))
            .on_press(Message::TabSelected(tab))
            .padding([8, 20]);

        if is_active {
            container(btn.style(button::primary))
                .style(|_theme| container::Style {
                    border: iced::Border {
                        color: color!(0x3d85c6),
                        width: 0.0,
                        radius: 0.into(),
                    },
                    background: Some(iced::Background::Color(color!(0x1e1e2e))),
                    ..Default::default()
                })
                .into()
        } else {
            container(btn.style(button::text)).into()
        }
    }

    // ── Tab content ────────────────────────────────────────────────────────

    fn view_tab_content(&self) -> Element<Message> {
        // Running tab gets its own layout with fixed toolbar + scrollable log
        if self.active_tab == Tab::Running {
            return self.view_running_tab();
        }

        let content: Element<Message> = match self.active_tab {
            Tab::Install => self.view_install_tab(),
            Tab::Update => self.view_update_tab(),
            Tab::Uninstall => self.view_uninstall_tab(),
            Tab::Troubleshoot => self.view_troubleshoot_tab(),
            Tab::Service => self.view_service_tab(),
            Tab::Running => unreachable!(),
        };

        container(
            scrollable(
                container(content).width(Length::Fill)
            )
            .height(Length::Fill)
            .width(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding { top: 10.0, right: 16.0, bottom: 6.0, left: 16.0 })
        .into()
    }

    fn view_install_tab(&self) -> Element<Message> {
        let mut steps_col = Column::new().spacing(6);

        for (step, status) in &self.steps {
            let indicator = text(status.indicator(self.spinner_tick))
                .size(16)
                .color(status.color())
                .width(20);

            let label = text(step_label(*step)).size(15);

            let mut row_items = row![indicator, label].spacing(10).align_y(iced::Alignment::Center);

            // Show detail text if we have it
            if let Some(detail) = status.detail() {
                row_items = row_items.push(
                    text(format!("— {}", detail))
                        .size(12)
                        .color(color!(0x7f849c)),
                );
            }

            // Live elapsed timer on the in-progress step — proves it's alive even
            // during a long silent download.
            if matches!(status, StepStatus::Installing) {
                if let Some(start) = self.step_started {
                    let secs = start.elapsed().as_secs();
                    let t = if secs < 60 { format!("{}s", secs) } else { format!("{}m{:02}s", secs / 60, secs % 60) };
                    row_items = row_items.push(text(t).size(12).color(color!(0x3d85c6)));
                }
            }

            steps_col = steps_col.push(row_items);
        }

        // Action buttons
        let has_not_found = self
            .steps
            .iter()
            .any(|(s, st)| *s != Step::Done && matches!(st, StepStatus::NotFound(_)));

        let all_not_started = self.steps.iter().all(|(_, st)| *st == StepStatus::NotStarted);

        let mut buttons_row = row![].spacing(10);

        // Scan button
        let scan_label = if all_not_started { "Scan System" } else { "Re-scan" };
        let scan_btn = button(text(scan_label))
            .on_press_maybe(if self.scanning || self.installing {
                None
            } else {
                Some(Message::ScanClicked)
            })
            .style(button::primary);
        buttons_row = buttons_row.push(scan_btn);

        // Go button — only if scan found stuff to install
        if has_not_found {
            let go_btn = button(
                text("Go — Install Missing").font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::DEFAULT
                }),
            )
            .on_press_maybe(if self.scanning || self.installing {
                None
            } else {
                Some(Message::GoClicked)
            })
            .style(button::success);
            buttons_row = buttons_row.push(go_btn);
        }

        column![
            text("Install Sapphire").size(18).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }),
            steps_col,
            buttons_row,
        ]
        .spacing(8)
        .padding(Padding { top: 0.0, right: 0.0, bottom: 12.0, left: 0.0 })
        .into()
    }

    fn view_update_tab(&self) -> Element<Message> {
        let mut steps_col = Column::new().spacing(6);

        for (label, status) in &self.update_status {
            let indicator = text(status.indicator(self.spinner_tick))
                .size(16)
                .color(status.color())
                .width(20);

            let label_text = text(label.as_str()).size(14);

            let mut row_items = row![indicator, label_text].spacing(10).align_y(iced::Alignment::Center);

            if let Some(detail) = status.detail() {
                row_items = row_items.push(
                    text(format!("— {}", detail)).size(11).color(color!(0x7f849c)),
                );
            }

            steps_col = steps_col.push(row_items);
        }

        let update_label = if self.updating {
            "Updating..."
        } else if self.sapphire_running || self.sapphire_stopping {
            "Stop & Update"
        } else {
            "Update"
        };

        let update_btn = button(text(update_label))
            .on_press_maybe(if self.updating || self.sapphire_stopping {
                None
            } else {
                Some(Message::UpdateClicked)
            })
            .style(button::primary);

        let status_text = match self.updates_available {
            Some(0) => text("Up to date.").size(13).color(color!(0x4caf50)),
            Some(n) => text(format!("{} update{} available.", n, if n == 1 { "" } else { "s" }))
                .size(13).color(color!(0x3d85c6)),
            None if self.checking_updates => text("Checking...").size(13).color(color!(0x7f849c)),
            None => text("Couldn't check for updates.").size(13).color(color!(0x7f849c)),
        };

        column![
            text("Update Sapphire").size(18).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }),
            status_text,
            steps_col,
            update_btn,
        ]
        .spacing(10)
        .into()
    }

    fn view_uninstall_tab(&self) -> Element<Message> {
        let busy = self.uninstalling;

        // ═══════════════════════════════════════════════════════════
        // Quick Resets — safe, non-destructive to the install
        // ═══════════════════════════════════════════════════════════

        let reset_pw_btn = button(text("Reset Password").size(13))
            .on_press(Message::ResetPassword)
            .style(button::primary)
            .padding([4, 12]);

        let reset_creds_btn = button(text("Reset API Keys").size(13))
            .on_press(Message::ResetCredentials)
            .style(button::primary)
            .padding([4, 12]);

        let resets_section = column![
            text("Quick Resets").size(16).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }),
            row![
                column![
                    reset_pw_btn,
                    text("Forgot your password? This clears it so Sapphire asks for a new one.")
                        .size(11).color(color!(0x7f849c)),
                ].spacing(3).width(Length::FillPortion(1)),
                column![
                    reset_creds_btn,
                    text("Clears saved API keys (Claude, OpenAI, etc). You'll re-enter them in Sapphire.")
                        .size(11).color(color!(0x7f849c)),
                ].spacing(3).width(Length::FillPortion(1)),
            ].spacing(16),
        ].spacing(8);

        // ═══════════════════════════════════════════════════════════
        // Danger Zone — destructive actions
        // ═══════════════════════════════════════════════════════════

        // Remove conda env
        let env_label = if self.confirm_remove_env { "YES, remove it" } else { "Remove conda env" };
        let remove_env_btn = button(text(env_label).size(13))
            .on_press_maybe(if busy { None } else { Some(Message::UninstallCondaEnvClick) })
            .style(button::danger)
            .padding([4, 12]);
        let env_desc = if self.confirm_remove_env {
            text("Click again to confirm.").size(11).color(color!(0xe74c3c))
        } else {
            text("Deletes the 'sapphire' Python environment and all packages.").size(11).color(color!(0x7f849c))
        };

        // Delete user data
        let ud_label = if self.confirm_delete_userdata { "YES, delete user data" } else { "Delete user data" };
        let delete_ud_btn = button(text(ud_label).size(13))
            .on_press_maybe(if busy { None } else { Some(Message::UninstallDeleteUserdataClick) })
            .style(button::danger)
            .padding([4, 12]);
        let ud_desc = if self.confirm_delete_userdata {
            text("Click again to confirm.").size(11).color(color!(0xe74c3c))
        } else {
            text("Removes sapphire/user/ — your settings and personal data.").size(11).color(color!(0x7f849c))
        };

        // Delete everything
        let folder_label = if self.confirm_delete_folder { "YES, delete everything" } else { "Delete Sapphire folder" };
        let delete_folder_btn = button(text(folder_label).size(13))
            .on_press_maybe(if busy { None } else { Some(Message::UninstallDeleteFolderClick) })
            .style(button::danger)
            .padding([4, 12]);
        let folder_desc = if self.confirm_delete_folder {
            text("FINAL WARNING. Everything will be permanently deleted.").size(11).color(color!(0xe74c3c))
        } else {
            text(format!("Nukes {} — code, settings, everything. Cannot be undone.", self.install_path))
                .size(11).color(color!(0x7f849c))
        };

        let danger_section = column![
            text("Danger Zone").size(16).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::DEFAULT
            }).color(color!(0xe74c3c)),
            text("These actions are destructive. Won't touch Git or Miniconda.").size(11).color(color!(0x7f849c)),
            column![remove_env_btn, env_desc].spacing(2),
            column![delete_ud_btn, ud_desc].spacing(2),
            column![delete_folder_btn, folder_desc].spacing(2),
        ].spacing(8);

        // Divider between sections
        let divider = container(text(""))
            .width(Length::Fill)
            .height(1)
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(color!(0x313244))),
                ..Default::default()
            });

        column![
            resets_section,
            divider,
            danger_section,
        ]
        .spacing(12)
        .into()
    }

    fn view_troubleshoot_tab(&self) -> Element<Message> {
        let mut checks_col = Column::new().spacing(8);

        for (check, status) in &self.ts_checks {
            const SPINNER: &[&str] = &["/", "-", "\\", "|"];
            let (indicator, color) = match status {
                TsStatus::NotChecked => ("-", color!(0x585b70)),
                TsStatus::Checking | TsStatus::Fixing => (SPINNER[self.spinner_tick % SPINNER.len()], color!(0x3d85c6)),
                TsStatus::Ok(_) | TsStatus::Fixed(_) => ("+", color!(0x4caf50)),
                TsStatus::Problem(_) => ("!", color!(0xe74c3c)),
            };

            let label_text = ts_label(*check);
            let detail = match status {
                TsStatus::Ok(s) | TsStatus::Problem(s) | TsStatus::Fixed(s) => Some(s.as_str()),
                TsStatus::Checking => Some("checking..."),
                TsStatus::Fixing => Some("fixing..."),
                TsStatus::NotChecked => None,
            };

            let mut check_row = row![
                text(indicator).size(14).color(color).width(18),
                text(label_text).size(14),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center);

            if let Some(d) = detail {
                check_row = check_row.push(
                    text(format!("— {}", d)).size(11).color(color!(0x7f849c))
                );
            }

            // Add Fix button for fixable problems
            let is_fixable = matches!(
                (check, status),
                (TsCheck::DepsHealth, TsStatus::Problem(_))
                    | (TsCheck::Plugins, TsStatus::Problem(_))
                    | (TsCheck::PluginDeps, TsStatus::Problem(_))
            );
            let fix_label = if matches!(check, TsCheck::Plugins | TsCheck::PluginDeps) { "Install" } else { "Fix" };
            if is_fixable {
                check_row = check_row.push(horizontal_space());
                check_row = check_row.push(
                    button(text(fix_label).size(11))
                        .on_press(Message::TroubleshootFix(*check))
                        .style(button::success)
                        .padding([2, 10])
                );
            }

            checks_col = checks_col.push(check_row);
        }

        let check_btn = button(text("Check Sapphire"))
            .on_press_maybe(if self.ts_running { None } else { Some(Message::TroubleshootCheck) })
            .style(button::primary);

        column![
            row![
                text("Troubleshoot").size(18).font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::DEFAULT
                }),
                horizontal_space(),
                check_btn,
            ].align_y(iced::Alignment::Center),
            checks_col,
        ]
        .spacing(10)
        .into()
    }

    #[cfg(not(windows))]
    fn view_service_tab(&self) -> Element<Message> {
        let bold = Font { weight: iced::font::Weight::Bold, ..Font::DEFAULT };
        let muted = color!(0x7f849c);
        let divider = || container(text("")).width(Length::Fill).height(1).style(|_t| container::Style {
            background: Some(iced::Background::Color(color!(0x313244))),
            ..Default::default()
        });

        // ── Manage view: a unit already exists ──────────────────────────────
        if let Some(info) = &self.service {
            // Running state (honest about crash-loops/failure), separate from autostart.
            let (status_txt, status_color) = if info.active_state == "active" {
                ("running", color!(0x4caf50))
            } else if info.active_state == "failed" {
                ("failed", color!(0xe74c3c))
            } else if info.active_state == "activating" || info.sub_state == "auto-restart" {
                ("restarting...", color!(0xf9e154))
            } else {
                ("stopped", muted)
            };
            let (auto_txt, auto_color) = if info.enabled {
                ("on", color!(0x4caf50))
            } else {
                ("off", muted)
            };
            let workdir = info.working_dir.clone().unwrap_or_else(|| "unknown".to_string());

            let run_row = if info.active {
                row![
                    button(text("Stop").size(13)).on_press(Message::ServiceStop).style(button::danger).padding([4, 14]),
                    button(text("Restart").size(13)).on_press(Message::ServiceRestart).style(button::primary).padding([4, 14]),
                ].spacing(10)
            } else {
                row![
                    button(text("Start").size(13)).on_press(Message::ServiceStart).style(button::success).padding([4, 14]),
                ].spacing(10)
            };

            let remove_label = if self.confirm_remove_service { "Click again to confirm removal" } else { "Remove service" };
            let remove_btn = button(text(remove_label).size(13))
                .on_press(Message::ServiceRemoveClick).style(button::danger).padding([4, 14]);

            return column![
                text("Autostart").size(18).font(bold),
                text("Sapphire runs as a systemd user service — starts at sign-in, restarts on crash. The header Launch/Stop control it too.")
                    .size(11).color(muted),
                row![text("Status:").size(14), text(status_txt).size(14).color(status_color)]
                    .spacing(8).align_y(iced::Alignment::Center),
                row![text("Autostart:").size(14), text(auto_txt).size(14).color(auto_color)]
                    .spacing(8).align_y(iced::Alignment::Center),
                run_row,
                row![
                    button(text("Start at sign-in").size(13)).on_press(Message::ServiceEnable).style(button::secondary).padding([4, 14]),
                    button(text("Don't start at sign-in").size(13)).on_press(Message::ServiceDisable).style(button::secondary).padding([4, 14]),
                ].spacing(10),
                text(format!("Folder: {}", workdir)).size(11).color(muted),
                divider(),
                text("Environment variables  (one KEY=VALUE per line)").size(13).font(bold),
                text("e.g. ANTHROPIC_API_KEY=…, OPENAI_API_KEY=… — one per line.").size(11).color(muted),
                text_editor(&self.env_content).on_action(Message::EnvEdit).height(Length::Fixed(120.0)),
                button(text("Save & restart").size(13)).on_press(Message::EnvSaveRestart).style(button::primary).padding([4, 14]),
                text("Written privately to ~/.config/sapphire/service.env (chmod 600).").size(11).color(muted),
                divider(),
                remove_btn,
                text("Backs up the unit to sapphire.service.bak, then deletes it.").size(11).color(muted),
            ].spacing(12).into();
        }

        // ── Pre-install view: no unit yet ───────────────────────────────────
        let main_py = PathBuf::from(&self.install_path).join("main.py").exists();
        let env_py = sapphire_env_python();
        let installed = main_py && env_py.is_some();

        let systemd_row = {
            let (mark, c, t) = if self.systemd_available {
                ("[ok]", color!(0x4caf50), "systemd user services: available".to_string())
            } else {
                ("[ ]", color!(0xf9e154), "systemd user services: not available on this system".to_string())
            };
            row![text(mark).size(13).color(c), text(t).size(13)].spacing(8).align_y(iced::Alignment::Center)
        };
        let install_row = {
            let (mark, c, t) = if installed {
                ("[ok]", color!(0x4caf50), format!("Sapphire install: found at {}", self.install_path))
            } else {
                ("[ ]", color!(0xf9e154), "Sapphire install: not found — install it first (Install tab)".to_string())
            };
            row![text(mark).size(13).color(c), text(t).size(13)].spacing(8).align_y(iced::Alignment::Center)
        };

        // Don't let users enable a service mid-install (it'd point at a half-installed Sapphire).
        let can_enable = self.systemd_available && installed && !self.installing && !self.scanning;
        let enable_btn = button(text("Enable Sapphire service").font(bold))
            .on_press_maybe(if can_enable { Some(Message::ServiceInstall) } else { None })
            .style(button::success);

        let will_run = if self.installing {
            "Finishing install — the button unlocks when it's done.".to_string()
        } else {
            match &env_py {
                Some(p) => format!("Will run:  {} main.py", p),
                None => "Will run:  (Sapphire environment not found)".to_string(),
            }
        };

        column![
            text("Autostart").size(18).font(bold),
            row![text("Sapphire service:").size(14), text("not set up").size(14).color(muted)]
                .spacing(8).align_y(iced::Alignment::Center),
            systemd_row,
            install_row,
            divider(),
            text("Run Sapphire as a background service").size(14).font(bold),
            text("systemd can start Sapphire when you sign in (and restart it if it crashes), running without the launcher open. Stops when you sign out.")
                .size(11).color(muted),
            text(will_run).size(11).color(muted),
            text("Locks in this folder — settle it first; re-create the service if you move Sapphire.").size(11).color(muted),
            enable_btn,
        ]
        .spacing(10)
        .into()
    }

    /// Windows autostart: a decoupled "start at sign-in" toggle (a logon Scheduled
    /// Task), separate from the header's Launch/Stop. Enabled-state = our wrapper .cmd
    /// existing. No process-ownership takeover like the Linux systemd path.
    #[cfg(windows)]
    fn view_service_tab(&self) -> Element<Message> {
        let bold = Font { weight: iced::font::Weight::Bold, ..Font::DEFAULT };
        let muted = color!(0x7f849c);
        let enabled = crate::platform::autostart_marker_path().exists();

        if enabled {
            let (run_txt, run_color) = if self.sapphire_running {
                ("running", color!(0x4caf50))
            } else {
                ("stopped", muted)
            };
            let remove_label = if self.confirm_remove_service {
                "Click again to confirm"
            } else {
                "Turn off autostart"
            };
            return column![
                text("Autostart").size(18).font(bold),
                text("Sapphire starts automatically when you sign in to Windows, with no console window. Use Launch / Stop (or the Log tab) to control it while it's running.")
                    .size(11).color(muted),
                row![text("Start at sign-in:").size(14), text("on").size(14).color(color!(0x4caf50))]
                    .spacing(8).align_y(iced::Alignment::Center),
                row![text("Sapphire:").size(14), text(run_txt).size(14).color(run_color)]
                    .spacing(8).align_y(iced::Alignment::Center),
                button(text(remove_label).size(13)).on_press(Message::ServiceRemoveClick).style(button::danger).padding([4, 14]),
                text("Windows will ask for permission (admin) to change this.").size(11).color(muted),
            ].spacing(12).into();
        }

        // Not set up yet.
        let installed = PathBuf::from(&self.install_path).join("main.py").exists()
            && crate::platform::sapphire_pythonw().is_some();
        let (mark, c, t) = if installed {
            ("[ok]", color!(0x4caf50), format!("Sapphire install: found at {}", self.install_path))
        } else {
            ("[ ]", color!(0xf9e154), "Sapphire install: not found — install it first (Install tab)".to_string())
        };
        let can_enable = installed && !self.installing && !self.scanning;
        let enable_btn = button(text("Enable at sign-in").font(bold))
            .on_press_maybe(if can_enable { Some(Message::ServiceInstall) } else { None })
            .style(button::success);

        column![
            text("Autostart").size(18).font(bold),
            row![text("Start at sign-in:").size(14), text("not set up").size(14).color(muted)]
                .spacing(8).align_y(iced::Alignment::Center),
            row![text(mark).size(13).color(c), text(t).size(13)].spacing(8).align_y(iced::Alignment::Center),
            text("Start Sapphire automatically when you sign in").size(14).font(bold),
            text("Runs Sapphire in the background at sign-in with no console window, and lets it restart itself if it crashes. Stops when you sign out.")
                .size(11).color(muted),
            text("Windows will ask for permission (admin) the first time you set this up.").size(11).color(muted),
            enable_btn,
        ]
        .spacing(10)
        .into()
    }

    fn view_running_tab(&self) -> Element<Message> {
        let status_text = if self.sapphire_running {
            text("Sapphire is running").size(13).color(color!(0x4caf50))
        } else if self.sapphire_log.is_empty() {
            text("Hit Launch to start Sapphire.").size(13).color(color!(0x7f849c))
        } else {
            text("Sapphire stopped").size(13).color(color!(0x7f849c))
        };

        let copy_btn = button(text("Copy").size(11))
            .on_press_maybe(if self.sapphire_log.is_empty() { None } else { Some(Message::CopyRunLog) })
            .style(button::secondary)
            .padding([2, 8]);

        let open_label = if cfg!(windows) { "Open in Notepad" } else { "Open log" };
        let open_btn = button(text(open_label).size(11))
            .on_press_maybe(if self.sapphire_log.is_empty() { None } else { Some(Message::OpenRunLog) })
            .style(button::secondary)
            .padding([2, 8]);

        let bottom_btn = button(text("Bottom").size(11))
            .on_press(Message::ScrollRunLog)
            .style(button::secondary)
            .padding([2, 8]);

        let toolbar = row![status_text, horizontal_space(), copy_btn, open_btn, bottom_btn]
            .spacing(6)
            .align_y(iced::Alignment::Center);

        // Render only the tail — re-shaping thousands of lines every redraw is the
        // expensive part. Copy/Open still use the full buffer.
        let log_text = if self.sapphire_log.is_empty() {
            "Waiting...".to_string()
        } else {
            let start = self.sapphire_log.len().saturating_sub(800);
            self.sapphire_log[start..].join("\n")
        };

        let log_scroll = scrollable(
            container(
                text(log_text).size(12).font(Font::MONOSPACE),
            )
            .width(Length::Fill)
            .padding(6),
        )
        .id(scrollable::Id::new("run-log"))
        .width(Length::Fill)
        .height(Length::Fill);

        let log_area = container(log_scroll)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(color!(0x11111b))),
                ..Default::default()
            });

        // Return the full layout — toolbar is fixed, log scrolls independently
        container(
            column![toolbar, log_area].spacing(4)
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding { top: 8.0, right: 12.0, bottom: 4.0, left: 12.0 })
        .into()
    }

    // ── Log panel ──────────────────────────────────────────────────────────

    fn view_log_panel(&self) -> Element<Message> {
        let toggle = button(if self.log_visible { "[-] Log" } else { "[+] Log" })
            .on_press(Message::ToggleLog)
            .style(button::text)
            .padding([2, 8]);

        let mut panel = column![toggle].spacing(2).width(Length::Fill);

        if self.log_visible {
            // Tail only — keep redraw cheap as the launcher log grows.
            let start = self.log_lines.len().saturating_sub(300);
            let log_text = self.log_lines[start..].join("\n");
            let log_area = container(
                scrollable(
                    container(
                        text(log_text)
                            .size(12)
                            .font(Font::MONOSPACE),
                    )
                    .width(Length::Fill)
                    .padding(6),
                )
                .anchor_bottom()
                .width(Length::Fill)
                .height(100),
            )
            .width(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(color!(0x11111b))),
                border: iced::Border {
                    color: color!(0x313244),
                    width: 1.0,
                    radius: 0.into(),
                },
                ..Default::default()
            });
            panel = panel.push(log_area);
        }

        container(panel)
            .width(Length::Fill)
            .padding(Padding { top: 8.0, right: 0.0, bottom: 0.0, left: 0.0 })
            .into()
    }
}
