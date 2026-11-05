//! SSH tunnel settings of a saved connection and the glue between the app and `engine-net`.
//!
//! The logic lives in `engine-connect` (`tunnel::ssh`), shared with `tablex-mcp`; this module keeps
//! the old path and adds the one piece that needs the UI executor: [`ChannelPrompts`], which
//! forwards a bastion's keyboard-interactive questions to the dialog.

use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use engine_net::{CancelToken, KeyboardInteractiveRequest, PromptItem, Prompts, Secret};

pub use engine_connect::tunnel::ssh::*;
#[cfg(any(test, feature = "ui-scenarios"))]
pub(crate) use engine_connect::tunnel::ssh::fake;

// ---------------------------------------------------------------------------------------------
// Keyboard-interactive prompts
// ---------------------------------------------------------------------------------------------

/// One question round of the bastion, handed to the UI.
pub struct KiPrompt {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub instructions: String,
    pub prompts: Vec<PromptItem>,
    /// Becomes cancelled when the open that asked was abandoned.
    pub cancel: CancelToken,
    /// `None` = the user cancelled.
    pub reply: std::sync::mpsc::Sender<Option<Vec<Secret>>>,
    /// The login the connection uses, for the dialog's fixed first line.
    pub user: String,
}

/// [`Prompts`] that forwards every question to the UI over a channel and blocks the (worker)
/// thread that asked until it is answered, cancelled or abandoned.
pub struct ChannelPrompts {
    sender: smol::channel::Sender<KiPrompt>,
    user: String,
}

impl ChannelPrompts {
    pub fn new(sender: smol::channel::Sender<KiPrompt>, user: String) -> Self {
        Self { sender, user }
    }
}

impl Prompts for ChannelPrompts {
    fn keyboard_interactive(&self, request: &KeyboardInteractiveRequest) -> Option<Vec<Secret>> {
        let (reply, answers) = std::sync::mpsc::channel();
        self.sender
            .send_blocking(KiPrompt {
                host: request.host.clone(),
                port: request.port,
                name: request.name.clone(),
                instructions: request.instructions.clone(),
                prompts: request.prompts.clone(),
                cancel: request.cancel.clone(),
                reply,
                user: self.user.clone(),
            })
            .ok()?;
        loop {
            match answers.recv_timeout(Duration::from_millis(200)) {
                Ok(answers) => {
                    return answers
                        .filter(|answers| answers.len() == request.prompts.len());
                }
                Err(RecvTimeoutError::Timeout) if request.cancel.is_cancelled() => return None,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_interactive_questions_reach_the_ui_and_the_answer_comes_back() {
        let (sender, receiver) = smol::channel::unbounded();
        let prompts = ChannelPrompts::new(sender, "deploy".into());
        let ui = std::thread::spawn(move || {
            let prompt = receiver.recv_blocking().unwrap();
            assert_eq!(prompt.prompts.len(), 2);
            assert!(!prompt.prompts[0].echo && prompt.prompts[1].echo);
            prompt.reply.send(Some(vec![Secret::new("123456"), Secret::new("laptop")])).unwrap();
        });
        let request = KeyboardInteractiveRequest {
            host: "bastion.example.com".into(),
            port: 22,
            name: "".into(),
            instructions: "".into(),
            prompts: vec![
                PromptItem { text: "Code".into(), echo: false },
                PromptItem { text: "Device".into(), echo: true },
            ],
            cancel: CancelToken::default(),
        };
        let answers = prompts.keyboard_interactive(&request).unwrap();
        ui.join().unwrap();
        assert_eq!(answers.iter().map(Secret::expose).collect::<Vec<_>>(), ["123456", "laptop"]);
    }

    #[test]
    fn a_cancelled_or_short_answer_declines_authentication() {
        for answer in [None, Some(vec![Secret::new("only-one")])] {
            let (sender, receiver) = smol::channel::unbounded();
            let prompts = ChannelPrompts::new(sender, "deploy".into());
            let ui = std::thread::spawn(move || {
                let prompt = receiver.recv_blocking().unwrap();
                prompt.reply.send(answer).unwrap();
            });
            let request = KeyboardInteractiveRequest {
                host: "h.example.com".into(),
                port: 22,
                name: String::new(),
                instructions: String::new(),
                prompts: vec![
                    PromptItem { text: "A".into(), echo: false },
                    PromptItem { text: "B".into(), echo: false },
                ],
                cancel: CancelToken::default(),
            };
            assert!(prompts.keyboard_interactive(&request).is_none());
            ui.join().unwrap();
        }
    }

    #[test]
    fn a_vanished_ui_declines_instead_of_hanging() {
        let (sender, receiver) = smol::channel::unbounded();
        drop(receiver);
        let prompts = ChannelPrompts::new(sender, "deploy".into());
        let request = KeyboardInteractiveRequest {
            host: "h.example.com".into(),
            port: 22,
            name: String::new(),
            instructions: String::new(),
            prompts: vec![PromptItem { text: "Code".into(), echo: false }],
            cancel: CancelToken::default(),
        };
        assert!(prompts.keyboard_interactive(&request).is_none());
    }
}
