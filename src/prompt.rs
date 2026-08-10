//! The one interactive surface in the tool: `wtree init` with no flags.
//!
//! Every other verb refuses and prints the flag to add, which is what makes it
//! usable from a script or an agent. `init` is the exception because it is the
//! command a person runs by hand, once, before there is anything to script
//! against. When there is no terminal it refuses like the rest.

use std::io::{self, IsTerminal};

use dialoguer::Select;
use dialoguer::console::Term;
use dialoguer::theme::ColorfulTheme;

/// stdin and *stderr*: dialoguer draws every prompt on `Term::stderr()`, so
/// stdout says nothing about whether a menu would be visible. Checking stdout
/// instead refuses `wtree init > log` from a terminal, and lets `wtree init
/// 2> log` through to die on a prompt it cannot draw.
pub fn available() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

/// Puts the cursor back before a Ctrl-C takes the process down.
///
/// console restores the terminal modes it changed and then re-raises SIGINT to
/// die at the conventional 130, which is right — but the hidden-cursor escape
/// dialoguer wrote is not a terminal mode, so nothing puts it back and the
/// user's shell is left with an invisible cursor. Nothing in the normal return
/// paths needs this; they show the cursor themselves.
struct CursorGuard(libc::sighandler_t);

extern "C" fn show_cursor_and_die(_sig: libc::c_int) {
    const SHOW: &[u8] = b"\x1b[?25h";
    // SAFETY: async-signal-safe only. One `write` to a fd nobody is closing,
    // then the default disposition and a re-raise so the exit status is the
    // 130 a shell expects from Ctrl-C. No allocation, no locks.
    unsafe {
        libc::write(libc::STDERR_FILENO, SHOW.as_ptr().cast(), SHOW.len());
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::raise(libc::SIGINT);
    }
}

impl CursorGuard {
    fn install() -> CursorGuard {
        // SAFETY: the handler is async-signal-safe (see above), and the
        // previous disposition is restored on drop.
        let handler = show_cursor_and_die as *const () as libc::sighandler_t;
        CursorGuard(unsafe { libc::signal(libc::SIGINT, handler) })
    }
}

impl Drop for CursorGuard {
    fn drop(&mut self) {
        // SAFETY: putting back exactly what `install` took out.
        unsafe { libc::signal(libc::SIGINT, self.0) };
    }
}

/// Arrow keys to move, Enter to pick. `None` when the user backs out with Esc.
/// Ctrl-C is not an answer: console re-raises it and the process exits 130
/// without returning here. Renders in place at the cursor and clears itself
/// afterwards, so the commands already on screen stay where they are.
fn ask(prompt: &str, items: &[String], default: usize) -> Result<Option<usize>, String> {
    let _guard = CursorGuard::install();
    Select::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .items(items)
        .default(default)
        .interact_opt()
        .map_err(|e| {
            // Any other read failure leaves the menu half-drawn, cursor and all.
            let _ = Term::stderr().show_cursor();
            format!("cannot read the answer: {e}")
        })
}

pub fn select(prompt: &str, items: &[String]) -> Result<Option<usize>, String> {
    ask(prompt, items, 0)
}

/// A two-item `Select` rather than dialoguer's `Confirm`, which wants a typed
/// y/n — the rest of the flow is arrow keys, and switching input styles mid-flow
/// reads as a bug. The cursor starts on `no`: this is only ever asked before
/// replacing something, and a reflexive Enter should not be what does it.
/// Backing out is a no as well.
pub fn confirm(prompt: &str) -> Result<bool, String> {
    let items = ["yes".to_string(), "no".to_string()];
    Ok(ask(prompt, &items, 1)? == Some(0))
}
