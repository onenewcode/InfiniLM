mod channels;

use android_logger::Config;
use channels::TaskRequest;
use jni::{
    objects::{JClass, JString},
    sys::jstring,
    JNIEnv,
};
use log::LevelFilter;
use service::Message;
use std::{
    path::PathBuf,
    sync::{
        mpsc::{channel as thread_mpsc, RecvError, TryRecvError},
        Once,
    },
};
use tokio::sync::mpsc::UnboundedReceiver;

type Service = service::Service<llama_cpu::Transformer>;

/// 加载模型并启动推理服务。
#[no_mangle]
pub extern "system" fn Java_org_infinitensor_lm_Native_init(
    mut env: JNIEnv,
    _: JClass,
    model_path: JString,
) {
    android_logger::init_once(
        Config::default()
            .with_max_level(LevelFilter::Info)
            .with_tag("Rust"),
    );

    static ONCE: Once = Once::new();
    if ONCE.is_completed() {
        panic!("Native library already initialized");
    }

    let model_dir: String = env
        .get_string(&model_path)
        .expect("Couldn't get java string!")
        .into();
    let model_dir = PathBuf::from(model_dir);

    if model_dir.is_dir() {
        ONCE.call_once(move || {
            let receiver = channels::init();
            std::thread::spawn(move || dispatch(model_dir, receiver));
        });
    } else {
        panic!("Model directory not found");
    }
}

/// 启动聊天会话。
#[no_mangle]
pub extern "system" fn Java_org_infinitensor_lm_Native_start(
    mut env: JNIEnv,
    _class: JClass,
    prompt: JString,
) {
    let prompt = env
        .get_string(&prompt)
        .expect("Couldn't get java string!")
        .into();
    let (sender, receiver) = thread_mpsc();
    channels::chat(prompt, sender);
    channels::dialog().lock().unwrap().replace(receiver);
}

///启动文本生成。
#[no_mangle]
pub extern "system" fn Java_org_infinitensor_lm_Native_startGenerate(
    mut env: JNIEnv,
    _class: JClass,
    prompt: JString,
) {
    let prompt = env
        .get_string(&prompt)
        .expect("Couldn't get java string!")
        .into();
    let (sender, receiver) = thread_mpsc();
    channels::generate(prompt, sender);
    channels::dialog().lock().unwrap().replace(receiver);
}

/// 终止生成。
#[no_mangle]
pub extern "system" fn Java_org_infinitensor_lm_Native_abort(_env: JNIEnv, _class: JClass) {
    let _ = channels::dialog().lock().unwrap().take();
}

/// 解码模型反馈。
#[no_mangle]
pub extern "system" fn Java_org_infinitensor_lm_Native_decode(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let mut lock = channels::dialog().lock().unwrap();
    let mut ans = String::new();
    if let Some(receiver) = lock.as_mut() {
        loop {
            match receiver.try_recv() {
                Ok(s) => ans.push_str(&s),
                Err(TryRecvError::Empty) => match receiver.recv() {
                    Ok(s) => {
                        ans.push_str(&s);
                        break;
                    }
                    Err(RecvError) => {
                        log::warn!("Receive disconnected");
                        lock.take();
                        break;
                    }
                },
                Err(TryRecvError::Disconnected) => {
                    log::warn!("Try receive disconnected");
                    lock.take();
                    break;
                }
            }
        }
    }
    env.new_string(&ans)
        .expect("Couldn't create java string!")
        .into_raw()
}

fn dispatch(model_dir: PathBuf, mut requests: UnboundedReceiver<TaskRequest>) {
    // 启动 tokio 运行时
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async move {
        let (service, _handle) = Service::load(model_dir, ());
        let mut session = None;
        while let Some(request) = requests.recv().await {
            match request {
                TaskRequest::Chat(content, answer) => {
                    log::info!("chat: {content}");
                    let session = session.get_or_insert_with(|| {
                        log::info!("new session");
                        service.launch()
                    });
                    session.extend(&[Message {
                        role: "user",
                        content: &content,
                    }]);
                    let mut chat = session.chat();
                    while let Some(piece) = chat.decode().await {
                        if answer.send(piece).is_err() {
                            log::warn!("send error");
                            break;
                        }
                    }
                    log::info!("chat finished");
                }
                TaskRequest::Generate(content, answer) => {
                    log::info!("generate: {content}");
                    let mut generator = service.generate(content, None);
                    while let Some(piece) = generator.decode().await {
                        if answer.send(piece).is_err() {
                            log::warn!("send error");
                            break;
                        }
                    }
                    log::info!("generation finished");
                }
            }
        }
    });
    // 关闭 tokio 运行时
    runtime.shutdown_background();
}
