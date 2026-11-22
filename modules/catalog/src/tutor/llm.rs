//! Вызов модели пользователя: OpenAI-совместимый `/chat/completions` или Anthropic
//! `/v1/messages`, потоком (SSE провайдера → куски текста в канал) и целиком (резюме, проверка).

use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Расшифрованный провайдер пользователя — только на время запроса, не логируется.
pub struct Provider {
    pub kind: String,
    pub base_url: Option<String>,
    pub model: String,
    pub key: String,
}

/// Реплика для модели.
pub struct Turn {
    pub role: &'static str,
    pub content: String,
}

/// Расход токенов по данным провайдера (если он их прислал).
#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub input: i32,
    pub output: i32,
}

fn anthropic(p: &Provider) -> bool {
    p.kind == "anthropic"
}

fn request(
    http: &reqwest::Client,
    p: &Provider,
    system: &str,
    turns: &[Turn],
    max_tokens: u32,
    stream: bool,
) -> reqwest::RequestBuilder {
    let messages: Vec<Value> = turns
        .iter()
        .map(|t| json!({ "role": t.role, "content": t.content }))
        .collect();
    if anthropic(p) {
        let base = p.base_url.as_deref().unwrap_or("https://api.anthropic.com");
        http.post(format!("{}/v1/messages", base.trim_end_matches('/')))
            .header("x-api-key", &p.key)
            .header("anthropic-version", "2023-06-01")
            .json(&json!({
                "model": p.model, "max_tokens": max_tokens, "system": system,
                "messages": messages, "stream": stream,
            }))
    } else {
        let base = p.base_url.as_deref().unwrap_or("https://api.openai.com/v1");
        let mut all = vec![json!({ "role": "system", "content": system })];
        all.extend(messages);
        let mut body = json!({
            "model": p.model, "max_tokens": max_tokens, "messages": all, "stream": stream,
        });
        if stream {
            // Расход токенов в последнем куске потока (OpenAI, DeepSeek, OpenRouter это понимают).
            body["stream_options"] = json!({ "include_usage": true });
        }
        http.post(format!("{}/chat/completions", base.trim_end_matches('/')))
            .bearer_auth(&p.key)
            .json(&body)
    }
}

/// Ответ не 2xx — короткое сообщение провайдера (ключ из текста вырезан).
async fn failure(p: &Provider, response: reqwest::Response) -> String {
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_default();
    let message = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("");
    format!(
        "HTTP {status}: {}",
        message
            .replace(&p.key, "***")
            .chars()
            .take(300)
            .collect::<String>()
    )
}

/// Ответ целиком.
pub async fn complete(
    http: &reqwest::Client,
    p: &Provider,
    system: &str,
    turns: &[Turn],
    max_tokens: u32,
) -> Result<(String, Usage), String> {
    let response = request(http, p, system, turns, max_tokens, false)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !response.status().is_success() {
        return Err(failure(p, response).await);
    }
    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    let text = body
        .pointer("/content/0/text")
        .or_else(|| body.pointer("/choices/0/message/content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let usage = Usage {
        input: int(&body, &["/usage/input_tokens", "/usage/prompt_tokens"]),
        output: int(&body, &["/usage/output_tokens", "/usage/completion_tokens"]),
    };
    Ok((text, usage))
}

fn int(v: &Value, pointers: &[&str]) -> i32 {
    pointers
        .iter()
        .find_map(|p| v.pointer(p).and_then(Value::as_i64))
        .unwrap_or(0) as i32
}

/// Ответ потоком: каждый кусок текста — в `tx`; в конце — весь текст и расход токенов.
/// Читатель ушёл (канал закрыт) — дочитываем ответ всё равно: он сохранится в обсуждении.
pub async fn stream(
    http: &reqwest::Client,
    p: &Provider,
    system: &str,
    turns: &[Turn],
    max_tokens: u32,
    tx: &mpsc::Sender<String>,
) -> Result<(String, Usage), String> {
    let response = request(http, p, system, turns, max_tokens, true)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !response.status().is_success() {
        return Err(failure(p, response).await);
    }
    let mut text = String::new();
    let mut usage = Usage::default();
    let mut buffer = Vec::<u8>::new();
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| e.without_url().to_string())?;
        buffer.extend_from_slice(&chunk);
        // SSE провайдера: строки «data: {...}»; неполную последнюю строку оставляем в буфере.
        while let Some(end) = buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(delta) = parse_delta(&event, &mut usage) {
                text.push_str(&delta);
                let _ = tx.send(delta).await;
            }
        }
    }
    Ok((text.trim().to_string(), usage))
}

/// Кусок текста из события потока (OpenAI или Anthropic); заодно — расход токенов.
fn parse_delta(event: &Value, usage: &mut Usage) -> Option<String> {
    // OpenAI: usage приходит в последнем куске (stream_options.include_usage).
    if let Some(u) = event.get("usage").filter(|u| !u.is_null()) {
        let input = int(u, &["/prompt_tokens", "/input_tokens"]);
        let output = int(u, &["/completion_tokens", "/output_tokens"]);
        if input > 0 {
            usage.input = input;
        }
        if output > 0 {
            usage.output = output;
        }
    }
    // Anthropic: message_start несёт входные токены.
    if let Some(input) = event
        .pointer("/message/usage/input_tokens")
        .and_then(Value::as_i64)
    {
        usage.input = input as i32;
    }
    event
        .pointer("/choices/0/delta/content")
        .or_else(|| event.pointer("/delta/text"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_and_usage_from_both_protocols() {
        let mut usage = Usage::default();
        let openai = json!({ "choices": [{ "delta": { "content": "При" } }] });
        assert_eq!(parse_delta(&openai, &mut usage).as_deref(), Some("При"));
        let last =
            json!({ "choices": [], "usage": { "prompt_tokens": 120, "completion_tokens": 40 } });
        assert_eq!(parse_delta(&last, &mut usage), None);
        assert_eq!((usage.input, usage.output), (120, 40));

        let mut usage = Usage::default();
        let start =
            json!({ "type": "message_start", "message": { "usage": { "input_tokens": 90 } } });
        parse_delta(&start, &mut usage);
        let delta = json!({ "type": "content_block_delta", "delta": { "type": "text_delta", "text": "вет" } });
        assert_eq!(parse_delta(&delta, &mut usage).as_deref(), Some("вет"));
        let end = json!({ "type": "message_delta", "usage": { "output_tokens": 33 } });
        parse_delta(&end, &mut usage);
        assert_eq!((usage.input, usage.output), (90, 33));
    }
}
