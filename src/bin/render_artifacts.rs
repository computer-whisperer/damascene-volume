//! Render every tab of `damascene-volume` against the demo backend and dump
//! Damascene bundle artifacts (svg + tree + draw ops + lint) to `out/`.
//!
//! Run:
//!   cargo run --bin render_artifacts
//!   cargo run --bin render_artifacts -- configuration
//!
//! With no args, every tab is rendered. With one or more tab names
//! (case-insensitive: playback, recording, outputs, inputs, configuration),
//! only those tabs are rendered.

use std::path::PathBuf;

use damascene_core::{App, BuildCx, Rect, render_bundle_themed, write_bundle};
use damascene_volume::{app::VolumeApp, backend::DemoBackend, model::Tab};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Mirror the runtime viewport (50% of a 1080p panel).
    let viewport = Rect::new(0.0, 0.0, 960.0, 1080.0);
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("out");

    let requested: Vec<Tab> = std::env::args()
        .skip(1)
        .map(|raw| parse_tab(&raw).unwrap_or_else(|| panic!("unknown tab `{raw}`")))
        .collect();
    let tabs: Vec<Tab> = if requested.is_empty() {
        Tab::ALL.to_vec()
    } else {
        requested
    };

    for tab in tabs {
        let app = VolumeApp::new(Box::new(DemoBackend)).with_active_tab(tab);
        let theme = app.theme();
        let mut tree = app.build(&BuildCx::new(&theme));
        let bundle = render_bundle_themed(&mut tree, viewport, &theme);
        let basename = artifact_basename(tab);
        let written = write_bundle(&bundle, &out_dir, &basename)?;
        for path in &written {
            println!("wrote {}", path.display());
        }

        if !bundle.lint.findings.is_empty() {
            eprintln!(
                "\n{basename} lint findings ({}):",
                bundle.lint.findings.len()
            );
            eprint!("{}", bundle.lint.text());
        }
    }

    Ok(())
}

fn parse_tab(raw: &str) -> Option<Tab> {
    Tab::ALL.into_iter().find(|tab| {
        tab.label().eq_ignore_ascii_case(raw) || artifact_basename(*tab) == raw.to_lowercase()
    })
}

fn artifact_basename(tab: Tab) -> String {
    format!("tab_{}", tab.token())
}
