//! `paos init` — a fresh clone to a working paos.
//!
//! Thin on purpose. Everything that can actually go wrong lives in a tested function
//! elsewhere: the model install (`paos_memory::model`), the secret write
//! (`paos_secrets::store`), and the id discovery (`paos_operator::telegram`). What is
//! left here is prompting and printing, which is the part a test cannot check anyway.

use std::io::Write;

/// What the wizard did, and what it did not.
///
/// Names the UNSET half too: a wizard that prints only its successes leaves someone
/// believing they are configured.
pub fn report(steps: &[(&str, bool)]) -> String {
    let mut out = String::from("\npaos init\n");
    for (name, ok) in steps {
        out.push_str(&format!("  {} {name}\n", if *ok { "✓" } else { "✗" }));
    }
    if steps.iter().any(|(n, ok)| *n == "telegram" && !ok) {
        out.push_str(
            "\nTelegram is optional — memory, the bus and the dashboard all work without \
             it. Re-run `paos init` when you want it.\n",
        );
    }
    if steps.iter().any(|(n, ok)| *n == "model" && !ok) {
        out.push_str(
            "\nWithout the model, recall falls back to a weak hash embedder. It works, \
             but it matches far less well — re-run `paos init` when you have a network.\n",
        );
    }
    out
}

fn prompt(question: &str) -> String {
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    line.trim().to_string()
}

pub fn run(_args: &[String]) -> i32 {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut steps: Vec<(&str, bool)> = Vec::new();

    let root = std::path::Path::new(&home).join(".paos");
    steps.push(("store", std::fs::create_dir_all(&root).is_ok()));

    let dir = std::path::Path::new(&home).join(".cache/paos/models/potion-retrieval-32M");
    println!("installing the embedding model (129 MB, once)…");
    let installed = match paos_memory::model::ensure(&dir) {
        paos_memory::model::Install::AlreadyPresent => {
            println!("  already there");
            true
        }
        paos_memory::model::Install::Installed => {
            println!("  done");
            true
        }
        paos_memory::model::Install::Failed(e) => {
            // Not fatal. Recall degrades to hash-v1, which is worse but works, and
            // abandoning the whole install over a download is the worse trade.
            println!("  could not install: {e}");
            false
        }
    };
    steps.push(("model", installed));

    let want = prompt("\nconfigure Telegram now? [y/N] ").to_lowercase();
    let mut tg = false;
    if want.starts_with('y') {
        println!("\nCreate a bot with @BotFather in Telegram, then paste its token here.");
        let token = prompt("token: ");
        if token.is_empty() {
            println!("  nothing pasted — skipping");
        } else {
            let env_path = root.join(".env");
            match paos_secrets::store(
                paos_secrets::default_backend(),
                "paos",
                "telegram_bot_token",
                &token,
                &env_path,
            ) {
                Ok(reference) => {
                    // The reference, never the token. Storing the value here would undo
                    // the entire point of the secrets layer.
                    let _ = set_config("telegram_bot_token", &reference);
                    println!(
                        "\nNow MESSAGE YOUR BOT — or add it to your group and post there. \
                         Waiting up to two minutes…"
                    );
                    match wait_for_first_message(&token) {
                        Some(d) => {
                            let _ = set_config("telegram_chat_id", &d.chat_id);
                            let _ = set_config("telegram_allowed_user_id",
                                               &d.user_id.to_string());
                            if let Some(u) = &d.username {
                                let _ = set_config("telegram_operator_username", u);
                            }
                            println!("  learned chat {} for user {}", d.chat_id, d.user_id);
                            tg = true;
                        }
                        None => println!("  nothing arrived — re-run when you are ready"),
                    }
                }
                Err(e) => println!("  could not store the token: {e}"),
            }
        }
    }
    steps.push(("telegram", tg));

    print!("{}", report(&steps));
    0
}

/// Poll for up to two minutes. Long enough to switch apps and type; short enough that a
/// wizard left running does not look hung.
///
/// No offset, so this reads the backlog. On a brand-new bot that is empty and correct;
/// on a bot used before it learns from an older message, which is still that operator's
/// own chat and so is still the right answer.
fn wait_for_first_message(token: &str) -> Option<paos_operator::telegram::Discovered> {
    let url = format!("https://api.telegram.org/bot{token}/getUpdates");
    for _ in 0..24 {
        if let Ok(out) = std::process::Command::new("curl")
            .args(["-sS", "--max-time", "10", &url])
            .output()
        {
            let body = String::from_utf8_lossy(&out.stdout);
            let updates = paos_operator::telegram::parse_updates(&body);
            if let Some(d) = paos_operator::telegram::discovered_from(&updates) {
                return Some(d);
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    None
}

/// Through the daemon, like every other write — re-exec'ing this same binary rather than
/// writing `paos_config` directly, because the daemon is the single writer.
fn set_config(key: &str, value: &str) -> Result<(), String> {
    let bin = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = std::process::Command::new(bin)
        .args(["config", "set", key, value])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_names_what_is_missing_not_only_what_is_set() {
        // A wizard that prints only its successes leaves someone believing they are
        // configured. The unset half is the actionable half.
        let r = report(&[("store", true), ("model", true), ("telegram", false)]);
        assert!(r.contains("✓ store"), "{r}");
        assert!(r.contains("✗ telegram"), "{r}");
    }

    #[test]
    fn telegram_is_optional_and_the_report_says_so() {
        // paos without Telegram is a working memory and bus. Treating the bridge as
        // mandatory would make people read a partial install as a broken one.
        let r = report(&[("store", true), ("model", true), ("telegram", false)]);
        assert!(r.to_lowercase().contains("optional"), "{r}");
    }

    #[test]
    fn a_missing_model_explains_what_it_costs() {
        // "✗ model" alone does not tell anyone that recall just got quietly worse.
        let r = report(&[("store", true), ("model", false), ("telegram", true)]);
        assert!(r.contains("weak hash embedder"), "{r}");
    }

    #[test]
    fn a_fully_configured_run_adds_no_warnings() {
        let r = report(&[("store", true), ("model", true), ("telegram", true)]);
        assert!(!r.contains("optional"), "{r}");
        assert!(!r.contains("weak hash"), "{r}");
    }
}
