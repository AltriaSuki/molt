use async_trait::async_trait;
use molt_proto::{Envelope, ServiceId};
use molt_transport::testkit::{self, Harness};
use molt_transport::unix::{UnixLink, UnixTransport};
use molt_transport::{Link, Secret, TransportError};

struct Unix {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    t: UnixTransport,
}

fn secret_for(id: &ServiceId) -> Secret {
    Secret::from_raw(format!("secret-of-{id}"))
}

#[async_trait]
impl Harness for Unix {
    type T = UnixTransport;

    async fn setup() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let t = UnixTransport::new(&path).unwrap();
        Self { _dir: dir, path, t }
    }

    fn transport(&self) -> &UnixTransport {
        &self.t
    }

    fn secret(&self, id: &ServiceId) -> Secret {
        secret_for(id)
    }

    async fn link(&self, id: &ServiceId, secret: &Secret) -> Result<Box<dyn Link>, TransportError> {
        Ok(Box::new(UnixLink::connect(&self.path, id, secret).await?))
    }

    async fn impersonate(&self, attacker: &ServiceId, victim: &ServiceId, msg: &Envelope) {
        // The only way in is the victim's socket, which demands the victim's secret.
        if let Ok(link) = UnixLink::connect(&self.path, victim, &secret_for(attacker)).await {
            let _ = link.send(msg).await;
        }
    }

    async fn eavesdrop(&self, attacker: &ServiceId, victim: &ServiceId) -> Option<Box<dyn Link>> {
        let link = UnixLink::connect(&self.path, victim, &secret_for(attacker)).await.ok()?;
        Some(Box::new(link))
    }

    fn reports_busy(&self) -> bool {
        true
    }

    fn reports_not_connected(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn unix_ordered_both_ways() {
    testkit::ordered_both_ways::<Unix>().await;
}

#[tokio::test]
async fn unix_large_messages_pass() {
    testkit::large_messages_pass::<Unix>().await;
}

#[tokio::test]
async fn unix_too_deep_messages_are_dropped() {
    testkit::too_deep_messages_are_dropped::<Unix>().await;
}

#[tokio::test]
async fn unix_sender_is_stamped() {
    testkit::sender_is_stamped_by_transport::<Unix>().await;
}

#[tokio::test]
async fn unix_wrong_secret_rejected() {
    testkit::wrong_secret_is_rejected::<Unix>().await;
}

#[tokio::test]
async fn unix_cannot_write_into_another_endpoint() {
    testkit::cannot_write_into_another_endpoint::<Unix>().await;
}

#[tokio::test]
async fn unix_cannot_read_another_inbox() {
    testkit::cannot_read_another_inbox::<Unix>().await;
}

#[tokio::test]
async fn unix_unconnected_service() {
    testkit::unconnected_service::<Unix>().await;
}

#[tokio::test]
async fn unix_full_inbox_is_bounded() {
    testkit::full_inbox_is_bounded::<Unix>().await;
}
