//! `spot mail-preview NAME --site S [--locale L] [--context FILE|-] [--format text|html|json]`:
//! renders a mail template with a JSON context and prints the result. Sends nothing, opens no
//! mail connection and needs no database: it reads the host configuration and the template
//! files only. Guide: `docs/running-spot/mail_templates.md`.
//!
//! The context is a JSON object read from `FILE` or standard input (`-`); without `--context`
//! the context is empty. Amounts, dates and times are one-key objects (`{"$decimal": "12.50"}`,
//! `{"$date": "2026-03-10"}`, `{"$datetime": "2026-03-10T14:45:00+05:00"}`).
//!
//! Output: `text` prints `Subject: ...`, a blank line and the text body (derived from the HTML
//! body when the template has none); `html` prints the HTML body; `json` prints the template,
//! the chosen locale, the subject and both bodies.
//!
//! Exit codes follow the host-wide convention: 0 done, 1 usage, 3 failed (the site, the
//! template or the context could not be used). Errors name the template, file and line, never
//! the context's values.

use std::io::Read;
use std::path::PathBuf;

use clap::{Arg, ArgMatches, Command};
use spot_mail_render::{MailRenderer, RenderError, TemplateRoots, context_from_json, html_to_text};

use super::{CommandCtx, Exit};

pub const NAME: &str = "mail-preview";

/// The most context JSON that is read: 1 MiB.
const MAX_CONTEXT_BYTES: u64 = 1024 * 1024;

pub fn command() -> Command {
    Command::new(NAME)
        .about("Render an e-mail template with a JSON context and print it; sends nothing")
        .arg(
            Arg::new("name")
                .value_name("NAME")
                .required(true)
                .help("The template name (the folder mail/<NAME>/ of the site or an app)"),
        )
        .arg(
            Arg::new("site")
                .long("site")
                .value_name("SITE")
                .required(true)
                .help("The site whose template folder and installed apps are searched"),
        )
        .arg(
            Arg::new("locale")
                .long("locale")
                .value_name("LOCALE")
                .help("The locale to render, for example ur-PK; falls back to en, then none"),
        )
        .arg(
            Arg::new("context")
                .long("context")
                .value_name("FILE|-")
                .help("A JSON object with the template's data; `-` reads standard input"),
        )
        .arg(
            Arg::new("format")
                .long("format")
                .value_name("FORMAT")
                .default_value("text")
                .help("What to print: text, html or json"),
        )
}

/// The name of the installed app at `dir`, from its `app.toml`, if it has one.
fn manifest_name(dir: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("app.toml")).ok()?;
    let table: toml::Table = text.parse().ok()?;
    table
        .get("app")?
        .as_table()?
        .get("name")?
        .as_str()
        .map(str::to_owned)
}

/// The roots searched: the site's template folder, then each installed app's `templates`.
fn roots_for(ctx: &mut CommandCtx, site: &str) -> spot_cli::Result<Vec<PathBuf>> {
    let installed = ctx.site(site)?.apps.clone();
    let (catalogue, template_root) = {
        let common = ctx.common()?;
        (
            common.apps.clone(),
            common.services.print.template_root.clone(),
        )
    };
    let root = ctx.root().to_path_buf();
    let mut roots = vec![root.join("sites").join(site).join(template_root)];
    for dir in catalogue {
        let dir = root.join(dir);
        // A site with no install list hosts every app of the catalogue; otherwise only the
        // apps it installed, in install order of the catalogue.
        let hosted = installed.is_empty()
            || manifest_name(&dir).is_some_and(|name| installed.contains(&name));
        if hosted {
            roots.push(dir.join("templates"));
        }
    }
    Ok(roots)
}

fn read_context(ctx: &CommandCtx, source: Option<&str>) -> Result<serde_json::Value, String> {
    let mut text = String::new();
    match source {
        None => return Ok(serde_json::json!({})),
        Some("-") => {
            std::io::stdin()
                .take(MAX_CONTEXT_BYTES + 1)
                .read_to_string(&mut text)
                .map_err(|_| "the context could not be read from standard input".to_owned())?;
        }
        Some(path) => {
            let file = std::fs::File::open(ctx.root().join(path))
                .or_else(|_| std::fs::File::open(path))
                .map_err(|_| "the context file could not be opened".to_owned())?;
            file.take(MAX_CONTEXT_BYTES + 1)
                .read_to_string(&mut text)
                .map_err(|_| "the context file could not be read".to_owned())?;
        }
    }
    if text.len() as u64 > MAX_CONTEXT_BYTES {
        return Err("the context is over 1 MiB".to_owned());
    }
    serde_json::from_str(&text).map_err(|_| "the context is not valid JSON".to_owned())
}

pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let (Some(name), Some(site)) = (
        matches.get_one::<String>("name"),
        matches.get_one::<String>("site"),
    ) else {
        writeln!(ctx.err(), "usage: spot mail-preview NAME --site SITE")?;
        return Ok(Exit::Usage);
    };
    let locale = matches.get_one::<String>("locale").map(String::as_str);
    let format = matches
        .get_one::<String>("format")
        .map_or("text", String::as_str);

    if !matches!(format, "text" | "html" | "json") {
        writeln!(ctx.err(), "--format must be text, html or json")?;
        return Ok(Exit::Usage);
    }
    let json = match read_context(
        ctx,
        matches.get_one::<String>("context").map(String::as_str),
    ) {
        Ok(json) => json,
        Err(message) => {
            writeln!(ctx.err(), "{message}")?;
            return Ok(Exit::Failed);
        }
    };
    let roots = roots_for(ctx, site)?;
    let rendered = context_from_json(json).and_then(|context| {
        MailRenderer::new(TemplateRoots::new(roots)).render(name, locale, &context)
    });
    let rendered = match rendered {
        Ok(rendered) => rendered,
        Err(error) => return fail(ctx, &error),
    };

    match format {
        "html" => match &rendered.html {
            Some(html) => writeln!(ctx.out(), "{html}")?,
            None => {
                writeln!(ctx.err(), "template {name} has no HTML body")?;
                return Ok(Exit::Failed);
            }
        },
        "json" => {
            let value = serde_json::json!({
                "template": rendered.template,
                "locale": rendered.locale,
                "subject": rendered.subject,
                "text": rendered.text,
                "html": rendered.html,
            });
            let text = serde_json::to_string_pretty(&value)
                .map_err(|_| spot_cli::Error("the preview could not be encoded".into()))?;
            writeln!(ctx.out(), "{text}")?;
        }
        _ => {
            let body = match (&rendered.text, &rendered.html) {
                (Some(text), _) => text.clone(),
                (None, Some(html)) => html_to_text(html),
                (None, None) => String::new(),
            };
            writeln!(ctx.out(), "Subject: {}\n\n{body}", rendered.subject)?;
        }
    }
    Ok(Exit::Ok)
}

fn fail(ctx: &mut CommandCtx, error: &RenderError) -> spot_cli::Result<Exit> {
    writeln!(ctx.err(), "{error}")?;
    Ok(Exit::Failed)
}
