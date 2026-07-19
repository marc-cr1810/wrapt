//! `wrapt why-not`: explain, in plain English, why a package can't be installed.
//!
//! This is the natural inverse of `wrapt why`. It simulates the install and, if
//! apt refuses, runs the failure through the same resolver that decorates every
//! other transaction's errors — turning apt's terse output into guidance.

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::{apt, resolve, ui};

pub fn run(package: &str) -> Result<()> {
    if apt::installed_set().contains(package) {
        ui::success(&format!(
            "{package} is already installed — run `wrapt why {package}` to see why."
        ));
        return Ok(());
    }

    match apt::simulate(&["install".to_string(), package.to_string()]) {
        // apt is happy: nothing stands in the way.
        Ok(tx) if !tx.is_empty() => {
            ui::success(&format!("{package} can be installed."));
            println!();
            ui::print_transaction(&tx, &apt::manual_set());
            println!();
            println!(
                "   Run {} to proceed.",
                format!("wrapt install {package}").cyan()
            );
            Ok(())
        }
        // Installable but a no-op — usually a virtual/provided package.
        Ok(_) => {
            ui::warn(&format!(
                "apt has nothing concrete to install for '{package}'. It may be a virtual \
                 package provided by another — try `wrapt search {package}`."
            ));
            Ok(())
        }
        // The interesting case: apt refused. Explain why.
        Err(e) => Err(resolve::explain(
            e,
            std::slice::from_ref(&package.to_string()),
        )),
    }
}
