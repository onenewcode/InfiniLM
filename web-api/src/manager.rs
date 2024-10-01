use crate::schemas::{
    AnonymousSessionId, DropSuccess, Drop_, Error, Fork, ForkSuccess, Infer, Sentence, SessionId,
};
use base64::{engine::general_purpose, Engine};
use causal_lm::CausalLM;
use service::{Message, Service, Session, SessionManager};
use std::sync::Arc;
use tokio::sync::mpsc::{self, UnboundedReceiver};

pub(crate) struct ServiceManager<M: CausalLM> {
    service: Service<M>,
    session_manager: SessionManager<SessionId, M>,
}

impl<M: CausalLM> ServiceManager<M> {
    #[inline]
    pub fn new(service: Service<M>, capacity: Option<usize>) -> Self {
        Self {
            service,
            session_manager: SessionManager::new(capacity),
        }
    }
}

impl<M> ServiceManager<M>
where
    M: CausalLM + Send + Sync + 'static,
    M::Storage: Send,
{
    pub fn infer(
        self: &Arc<Self>,
        Infer {
            mut messages,
            encoding,
            session_id,
            dialog_pos,
            temperature,
            top_k,
            top_p,
        }: Infer,
    ) -> Result<UnboundedReceiver<String>, Error> {
        match encoding.as_deref() {
            Some("base64") | None => {
                for m in &mut messages {
                    let content = m.content.as_str();
                    m.content = general_purpose::STANDARD
                        .decode(content)
                        .map(String::from_utf8)
                        .map_err(|_| Error::InvalidContent(format!("Decode failed: {content}")))?
                        .map_err(|_| Error::InvalidContent(format!("Decode failed: {content}")))?;
                }
            }
            Some("text") => {}
            Some(e) => return Err(Error::InvalidContent(format!("Unknown encoding: {e}"))),
        };

        async fn infer<M: CausalLM>(
            session_id: &SessionId,
            session: &mut Session<M>,
            messages: Vec<Sentence>,
            temperature: Option<f32>,
            top_k: Option<usize>,
            top_p: Option<f32>,
            sender: mpsc::UnboundedSender<String>,
        ) {
            if let Some(temperature) = temperature {
                session.sample.temperature = temperature;
            }
            if let Some(top_k) = top_k {
                session.sample.top_k = top_k;
            }
            if let Some(top_p) = top_p {
                session.sample.top_p = top_p;
            }

            let messages = messages
                .iter()
                .map(|s| Message {
                    role: &s.role,
                    content: &s.content,
                })
                .collect::<Vec<_>>();
            session.extend(&messages);
            if session.dialog_pos() % 2 == 1 {
                info!("{session_id:?} inference started");
                let mut busy = session.chat();
                while let Some(s) = busy.decode().await {
                    if let Err(e) = sender.send(s) {
                        warn!("Failed to send piece to {session_id:?} with error \"{e}\"");
                        break;
                    }
                }
                info!("{session_id:?} inference stopped");
            } else {
                info!("{session_id:?} inference skipped");
            }
        }

        match (session_id, dialog_pos.unwrap_or(0)) {
            (Some(session_id_str), 0) => {
                let session_id = SessionId::Permanent(session_id_str);
                let mut session = self
                    .session_manager
                    .take_or_register(session_id.clone(), || self.service.launch())
                    .map_err(Error::Session)?;
                let (sender, receiver) = mpsc::unbounded_channel();
                let self_ = self.clone();
                tokio::spawn(async move {
                    session.revert(0).unwrap();
                    infer(
                        &session_id,
                        &mut session,
                        messages,
                        temperature,
                        top_k,
                        top_p,
                        sender,
                    )
                    .await;

                    self_.session_manager.restore(&session_id, session);
                });
                Ok(receiver)
            }
            (Some(session_id_str), p) => {
                let session_id = SessionId::Permanent(session_id_str);
                let mut session = self
                    .session_manager
                    .take(&session_id)
                    .map_err(Error::Session)?;
                if session.revert(p).is_err() {
                    let current = session.dialog_pos();
                    warn!(
                        "Failed to revert {session_id:?} from {current} to {p}, session restored"
                    );
                    self.session_manager.restore(&session_id, session);
                    return Err(Error::InvalidDialogPos(current));
                }
                let (sender, receiver) = mpsc::unbounded_channel();
                let self_ = self.clone();
                tokio::spawn(async move {
                    info!("{session_id:?} reverted to {p}");
                    infer(
                        &session_id,
                        &mut session,
                        messages,
                        temperature,
                        top_k,
                        top_p,
                        sender,
                    )
                    .await;

                    self_.session_manager.restore(&session_id, session);
                });
                Ok(receiver)
            }
            (None, 0) => {
                let session_id = SessionId::Temporary(AnonymousSessionId::new());
                let mut session = self
                    .session_manager
                    .take_or_register(session_id.clone(), || self.service.launch())
                    .map_err(Error::Session)?;
                let (sender, receiver) = mpsc::unbounded_channel();
                let self_ = self.clone();
                if messages.len() % 2 == 1 {
                    tokio::spawn(async move {
                        infer(
                            &session_id,
                            &mut session,
                            messages,
                            temperature,
                            top_k,
                            top_p,
                            sender,
                        )
                        .await;
                        self_.session_manager.drop_(&session_id).unwrap();
                    });
                }
                Ok(receiver)
            }
            (None, _) => {
                warn!("Temporary session must be created with zero dialog position");
                Err(Error::InvalidDialogPos(0))
            }
        }
    }

    pub fn fork(
        &self,
        Fork {
            session_id,
            new_session_id,
        }: Fork,
    ) -> Result<ForkSuccess, Error> {
        self.session_manager
            .fork(session_id.into(), new_session_id.into())
            .map(|()| ForkSuccess)
            .map_err(Error::Session)
    }

    pub fn drop_(&self, Drop_ { session_id }: Drop_) -> Result<DropSuccess, Error> {
        self.session_manager
            .drop_(&session_id.into())
            .map(|()| DropSuccess)
            .map_err(Error::Session)
    }
}
