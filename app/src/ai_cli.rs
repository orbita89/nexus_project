//! `nexus ai ...` — провайдеры ИИ из терминала (то же, что `/admin/ai/providers`).

use catalog::ai::{self, AiEffort, AiKind, AiProviderInput};
use shared::AppState;

pub const USAGE: &str = "usage:\n  \
     nexus ai list\n  \
     nexus ai add SLUG --kind anthropic|openai --model M --keys K1,K2 [--url URL] [--workers N] [--per-request N] [--effort none|low|medium|high] [--default]\n  \
     nexus ai set SLUG [--kind ..] [--model ..] [--keys ..] [--url URL|''] [--workers N] [--per-request N] [--effort none|low|medium|high] [--default]\n  \
     nexus ai test SLUG\n  \
     nexus ai rm SLUG";

fn input(args: &[String]) -> Result<AiProviderInput, String> {
    let mut out = AiProviderInput::default();
    let mut rest = args.iter();
    let number = |v: Option<&String>| -> Result<i32, String> {
        v.and_then(|v| v.parse().ok())
            .ok_or_else(|| USAGE.to_string())
    };
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--kind" => {
                out.kind = Some(
                    rest.next()
                        .and_then(|k| AiKind::parse(k))
                        .ok_or("--kind anthropic|openai")?,
                )
            }
            "--model" => out.model = Some(rest.next().ok_or(USAGE)?.clone()),
            "--url" => out.base_url = Some(rest.next().ok_or(USAGE)?.clone()),
            "--keys" => {
                out.api_keys = Some(
                    rest.next()
                        .ok_or(USAGE)?
                        .split(',')
                        .map(str::to_string)
                        .collect(),
                )
            }
            "--workers" => out.workers = Some(number(rest.next())?),
            "--per-request" => out.per_request = Some(number(rest.next())?),
            "--effort" => {
                out.effort = Some(
                    rest.next()
                        .and_then(|e| AiEffort::parse(e))
                        .ok_or("--effort none|low|medium|high")?,
                )
            }
            "--default" => out.is_default = Some(true),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(out)
}

fn show(p: &ai::AiProvider) {
    println!(
        "{}{}  {}  {}  model={}  keys={} ({})  workers={}  per_request={}  effort={}",
        if p.is_default { "* " } else { "  " },
        p.slug,
        p.kind,
        p.base_url.as_deref().unwrap_or("-"),
        p.model,
        p.keys_count,
        p.keys_hint,
        p.workers,
        p.per_request,
        p.effort
    );
}

pub async fn run(state: &AppState, args: &[String]) -> anyhow::Result<()> {
    let err = |e: shared::AppError| anyhow::anyhow!("{e}");
    match args {
        [cmd] if cmd == "list" => {
            let all = ai::list(state).await.map_err(err)?;
            if all.is_empty() {
                println!("провайдеров нет: nexus ai add ...");
            }
            all.iter().for_each(show);
        }
        [cmd, slug, rest @ ..] if cmd == "add" => {
            let p = ai::create(state, slug, input(rest).map_err(anyhow::Error::msg)?)
                .await
                .map_err(err)?;
            show(&p);
        }
        [cmd, slug, rest @ ..] if cmd == "set" => {
            let p = ai::update(state, slug, input(rest).map_err(anyhow::Error::msg)?)
                .await
                .map_err(err)?;
            show(&p);
        }
        [cmd, slug] if cmd == "test" => {
            let t = ai::test(state, slug).await.map_err(err)?;
            for (i, e) in t.errors.iter().enumerate() {
                match e {
                    None => println!("ключ {}: ok", i + 1),
                    Some(e) => println!("ключ {}: {e}", i + 1),
                }
            }
            if let Some(reply) = t.reply {
                println!("ответ модели: {reply}");
            }
            if !t.ok {
                anyhow::bail!("не все ключи работают");
            }
        }
        [cmd, slug] if cmd == "rm" => ai::delete(state, slug).await.map_err(err)?,
        _ => anyhow::bail!(USAGE),
    }
    Ok(())
}
