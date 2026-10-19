use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    EmergencyKey,
}

pub struct StopSignal {
    receiver: watch::Receiver<Option<StopReason>>,
}

#[derive(Clone)]
pub struct StopNotifier {
    sender: watch::Sender<Option<StopReason>>,
}

impl StopSignal {
    pub fn channel() -> (StopNotifier, Self) {
        let (sender, receiver) = watch::channel(None);
        (StopNotifier { sender }, Self { receiver })
    }

    pub async fn wait(&mut self) -> StopReason {
        let _ = self.receiver.wait_for(Option::is_some).await;
        self.receiver.borrow().unwrap_or(StopReason::EmergencyKey)
    }
}

impl StopNotifier {
    pub fn request_stop(&self, reason: StopReason) {
        self.sender.send_replace(Some(reason));
    }
}
