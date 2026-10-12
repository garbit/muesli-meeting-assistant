//! Chat-template rendering. Models ship a Jinja template in their GGUF
//! metadata; llama.cpp's built-in matcher only knows a fixed set, so we render
//! the model's own template with minijinja (with Python-compat helpers) and
//! fall back to a hand-written Gemma 4 layout.

use anyhow::{Context, Result};
use minijinja::{context, Environment};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(s: impl Into<String>) -> Self {
        Self { role: "system".into(), content: s.into() }
    }
    pub fn user(s: impl Into<String>) -> Self {
        Self { role: "user".into(), content: s.into() }
    }
}

/// A parsed chat template, reusable across renders (parsing a 20k-char Jinja
/// template costs a few ms; models are rendered twice per summary or more).
pub struct Renderer {
    env: Environment<'static>,
    bos: String,
}

impl Renderer {
    pub fn new(template: &str, bos: &str) -> Result<Self> {
        let mut env = Environment::new();
        env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
        minijinja_contrib::add_to_environment(&mut env);
        env.add_template_owned("chat".to_string(), template.to_string()).context("parsing chat template")?;
        Ok(Self { env, bos: bos.to_string() })
    }

    pub fn render(&self, messages: &[ChatMessage]) -> Result<String> {
        let tmpl = self.env.get_template("chat")?;
        let messages = minijinja::Value::from(minijinja::value::Serde(messages.to_vec()));
        let out = tmpl
            .render(context! {
                messages => messages,
                add_generation_prompt => true,
                enable_thinking => false,
                bos_token => self.bos.as_str(),
                eos_token => "",
                tools => minijinja::Value::UNDEFINED,
            })
            .context("rendering chat template")?;
        Ok(out)
    }
}

/// Render `messages` with the model's Jinja template. `bos` is the textual BOS
/// token (e.g. `<bos>`), inserted by templates that reference `bos_token`.
pub fn render_jinja(template: &str, messages: &[ChatMessage], bos: &str) -> Result<String> {
    Renderer::new(template, bos)?.render(messages)
}

/// Gemma 4 canonical layout, used when the model's template cannot be rendered.
pub fn render_gemma4(messages: &[ChatMessage], bos: &str) -> String {
    let mut out = String::from(bos);
    let mut iter = messages.iter().peekable();
    if let Some(first) = iter.peek()
        && first.role == "system" {
            out.push_str("<|turn>system\n");
            out.push_str(first.content.trim());
            out.push_str("<turn|>\n");
            iter.next();
        }
    for m in iter {
        let role = if m.role == "assistant" { "model" } else { m.role.as_str() };
        out.push_str(&format!("<|turn>{role}\n{}<turn|>\n", m.content.trim()));
    }
    out.push_str("<|turn>model\n<|channel>thought\n<channel|>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemma4_fallback_layout() {
        let p = render_gemma4(&[ChatMessage::system("S"), ChatMessage::user("U")], "<bos>");
        assert_eq!(p, "<bos><|turn>system\nS<turn|>\n<|turn>user\nU<turn|>\n<|turn>model\n<|channel>thought\n<channel|>");
    }

    #[test]
    fn jinja_renders_simple_template() {
        let t = "{{ bos_token }}{% for m in messages %}<{{ m.role }}>{{ m.content | trim }}</{{ m.role }}>{% endfor %}{% if add_generation_prompt %}<assistant>{% endif %}";
        let p = render_jinja(t, &[ChatMessage::user(" hi ")], "<s>").unwrap();
        assert_eq!(p, "<s><user>hi</user><assistant>");
    }

    #[test]
    fn jinja_supports_python_methods_and_namespace() {
        let t = "{% set ns = namespace(n=0) %}{% for m in messages %}{% if m.get('content') is string %}{% set ns.n = ns.n + 1 %}{% endif %}{% endfor %}{{ ns.n }}:{{ messages[0]['content'].strip() }}";
        let p = render_jinja(t, &[ChatMessage::user(" a "), ChatMessage::user("b")], "").unwrap();
        assert_eq!(p, "2:a");
    }
}
