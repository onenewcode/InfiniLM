#![deny(warnings)]

use minijinja::Environment;
use serde::Serialize;
use std::sync::{
    atomic::{AtomicUsize, Ordering::Relaxed},
    OnceLock, RwLock,
};

#[repr(transparent)]
pub struct ChatTemplate(String);

#[derive(Serialize)]
pub struct Message<'a> {
    pub role: &'a str,
    pub content: &'a str,
}

impl ChatTemplate {
    pub fn new(template: String) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, Relaxed).to_string();

        jinja()
            .write()
            .unwrap()
            .add_template_owned(id.clone(), template)
            .unwrap();

        Self(id)
    }

    pub fn render(
        &self,
        messages: &[Message<'_>],
        bos_token: &str,
        eos_token: &str,
        add_generation_prompt: bool,
    ) -> Result<String, minijinja::Error> {
        #[derive(Serialize)]
        struct Args<'a> {
            messages: &'a [Message<'a>],
            bos_token: &'a str,
            eos_token: &'a str,
            add_generation_prompt: bool,
        }

        jinja()
            .read()
            .unwrap()
            .get_template(&self.0)
            .unwrap()
            .render(Args {
                messages,
                bos_token,
                eos_token,
                add_generation_prompt,
            })
    }
}

impl Drop for ChatTemplate {
    fn drop(&mut self) {
        jinja().write().unwrap().remove_template(&self.0);
    }
}

fn jinja() -> &'static RwLock<Environment<'static>> {
    static ENV: OnceLock<RwLock<Environment<'_>>> = OnceLock::new();
    ENV.get_or_init(|| {
        let mut env = Environment::empty();
        env.set_unknown_method_callback(|_, value, method, args| {
            use minijinja::{value::ValueKind as ThisType, ErrorKind::UnknownMethod, Value};
            match (method, value.kind(), args) {
                ("strip", ThisType::String, []) => Ok(Value::from_safe_string(
                    value.to_str().unwrap().trim().into(),
                )),
                _ => Err(UnknownMethod.into()),
            }
        });
        RwLock::new(env)
    })
}

#[test]
fn test() {
    const TAIDE: &str = "{% if messages[0]['role'] == 'system' %}{% set loop_messages = messages[1:] %}{% set system_message = '<<SYS>>\n' + messages[0]['content'] + '\n<</SYS>>\n\n' %}{% else %}{% set loop_messages = messages %}{% set system_message = '' %}{% endif %}{% for message in loop_messages %}{% if loop.index0 == 0 %}{% set content = system_message + message['content'] %}{% else %}{% set content = message['content'] %}{% endif %}{% if message['role'] == 'user' %}{{ bos_token + '[INST] ' + content + ' [/INST]'}}{% elif message['role'] == 'assistant' %}{{ ' '  + content + ' ' + eos_token }}{% endif %}{% endfor %}{% if add_generation_prompt %}{{ '<|im_start|>assistant\n' }}{% endif %}";
    const MINICPM: &str = "{% for message in messages %}{% if message['role'] == 'user' %}{{'<用户>' + message['content'].strip() + '<AI>'}}{% else %}{{message['content'].strip()}}{% endif %}{% endfor %}";

    let result = ChatTemplate::new(TAIDE.into())
        .render(
            &[Message {
                role: "user",
                content: "Hello, who are you?",
            }],
            "<s>",
            "</s>",
            true,
        )
        .unwrap();

    assert_eq!(
        result,
        "<s>[INST] Hello, who are you? [/INST]<|im_start|>assistant\n"
    );

    let result = ChatTemplate::new(MINICPM.into())
        .render(
            &[Message {
                role: "user",
                content: "Hello, who are you?",
            }],
            "<s>",
            "</s>",
            true,
        )
        .unwrap();
    assert_eq!(result, "<用户>Hello, who are you?<AI>");
}
