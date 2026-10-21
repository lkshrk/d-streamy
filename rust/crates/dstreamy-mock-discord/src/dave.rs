//! Server side of DAVE: Discord's MLS external sender plus a simulated participant the client adds.

use std::num::NonZeroU16;
use std::time::{Duration, Instant};

use davey::{DaveSession, MediaType};
use openmls::prelude::tls_codec::{
    Deserialize as TlsDeserialize, DeserializeBytes, Serialize as TlsSerialize,
};
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use thiserror::Error;

pub const DAVE_MAGIC: [u8; 2] = [0xFA, 0xFA];

/// How long receivers still accept plaintext after a transition to E2EE (davey's default).
pub const TRANSITION_EXPIRY: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum DaveServerError {
    #[error("MLS: {0}")]
    Mls(String),
    #[error("davey: {0}")]
    Davey(String),
    #[error("key package belongs to user {0}, expected {1}")]
    WrongUser(u64, u64),
    #[error("no participant session")]
    NoParticipant,
}

fn mls(e: impl std::fmt::Display) -> DaveServerError {
    DaveServerError::Mls(e.to_string())
}

fn davey(e: impl std::fmt::Display) -> DaveServerError {
    DaveServerError::Davey(e.to_string())
}

pub struct DaveServer {
    protocol_version: NonZeroU16,
    client_user_id: u64,
    channel_id: u64,
    participant_id: u64,
    provider: OpenMlsRustCrypto,
    signer: SignatureKeyPair,
    external_sender: Vec<u8>,
    participant: Option<DaveSession>,
    next_transition: u16,
    /// `None` until a transition executes: plaintext is accepted without limit.
    pub(crate) plaintext_until: Option<Instant>,
}

impl DaveServer {
    pub fn new(
        protocol_version: NonZeroU16,
        client_user_id: u64,
        channel_id: u64,
        participant_id: u64,
    ) -> Result<Self, DaveServerError> {
        let signer = SignatureKeyPair::new(SignatureScheme::ECDSA_SECP256R1_SHA256).map_err(mls)?;
        let credential = BasicCredential::new(b"discord".to_vec());
        let external_sender =
            ExternalSender::new(signer.public().to_vec().into(), credential.into())
                .tls_serialize_detached()
                .map_err(mls)?;
        Ok(Self {
            protocol_version,
            client_user_id,
            channel_id,
            participant_id,
            provider: OpenMlsRustCrypto::default(),
            signer,
            external_sender,
            participant: None,
            next_transition: 0,
            plaintext_until: None,
        })
    }

    pub fn external_sender_package(&self) -> &[u8] {
        &self.external_sender
    }

    pub fn participant_id(&self) -> u64 {
        self.participant_id
    }

    pub fn on_key_package(&mut self, key_package: &[u8]) -> Result<Vec<u8>, DaveServerError> {
        let client_kp = parse_key_package(&self.provider, key_package)?;
        let owner = credential_user_id(client_kp.leaf_node().credential())?;
        if owner != self.client_user_id {
            return Err(DaveServerError::WrongUser(owner, self.client_user_id));
        }

        let mut participant = DaveSession::new(
            self.protocol_version,
            self.participant_id,
            self.channel_id,
            None,
        )
        .map_err(davey)?;
        participant
            .set_external_sender(&self.external_sender)
            .map_err(davey)?;
        let participant_kp = participant.create_key_package().map_err(davey)?;
        let participant_kp = parse_key_package(&self.provider, &participant_kp)?;
        self.participant = Some(participant);

        let proposal = ExternalProposal::new_add::<OpenMlsRustCrypto>(
            participant_kp,
            GroupId::from_slice(&self.channel_id.to_be_bytes()),
            GroupEpoch::from(0),
            &self.signer,
            SenderExtensionIndex::new(0),
        )
        .map_err(mls)?
        .tls_serialize_detached()
        .map_err(mls)?;
        let mut payload = vec![0u8];
        payload.extend(
            VLBytes::new(proposal)
                .tls_serialize_detached()
                .map_err(mls)?,
        );
        Ok(payload)
    }

    pub fn on_commit_welcome(&mut self, payload: &[u8]) -> Result<(u16, Vec<u8>), DaveServerError> {
        let (_, welcome) = MlsMessageIn::tls_deserialize_bytes(payload).map_err(mls)?;
        let commit = &payload[..payload.len() - welcome.len()];
        let participant = self
            .participant
            .as_mut()
            .ok_or(DaveServerError::NoParticipant)?;
        participant.process_welcome(welcome).map_err(davey)?;
        self.next_transition = self.next_transition.wrapping_add(1).max(1);
        let transition_id = self.next_transition;
        let mut out = transition_id.to_be_bytes().to_vec();
        out.extend_from_slice(commit);
        Ok((transition_id, out))
    }

    pub fn execute_transition(&mut self) {
        let until = Instant::now() + TRANSITION_EXPIRY;
        self.plaintext_until = Some(self.plaintext_until.map_or(until, |t| t.min(until)));
    }

    pub fn accepts_plaintext(&self) -> bool {
        self.plaintext_until.is_none_or(|t| Instant::now() < t)
    }

    pub fn is_active(&self) -> bool {
        self.participant.as_ref().is_some_and(DaveSession::is_ready)
    }

    pub fn privacy_code(&self) -> Option<String> {
        self.participant
            .as_ref()
            .and_then(|p| p.voice_privacy_code().map(str::to_owned))
    }

    pub fn decrypt(
        &mut self,
        media_type: MediaType,
        frame: &[u8],
    ) -> Result<Vec<u8>, DaveServerError> {
        let participant = self
            .participant
            .as_mut()
            .ok_or(DaveServerError::NoParticipant)?;
        participant
            .decrypt(self.client_user_id, media_type, frame)
            .map_err(davey)
    }
}

pub fn is_dave_frame(frame: &[u8]) -> bool {
    frame.ends_with(&DAVE_MAGIC)
}

fn parse_key_package(
    provider: &OpenMlsRustCrypto,
    bytes: &[u8],
) -> Result<KeyPackage, DaveServerError> {
    KeyPackageIn::tls_deserialize_exact(bytes)
        .map_err(mls)?
        .validate(provider.crypto(), ProtocolVersion::Mls10)
        .map_err(mls)
}

fn credential_user_id(credential: &Credential) -> Result<u64, DaveServerError> {
    let bytes: [u8; 8] = credential
        .serialized_content()
        .try_into()
        .map_err(|_| mls("credential is not a user id"))?;
    Ok(u64::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use davey::Codec;

    #[test]
    fn client_session_becomes_ready_and_frames_decrypt() {
        let v = NonZeroU16::new(1).unwrap();
        let (client_id, channel_id) = (42u64, 7u64);
        let mut server = DaveServer::new(v, client_id, channel_id, 9999).unwrap();
        let mut client = DaveSession::new(v, client_id, channel_id, None).unwrap();
        let kp = client.create_key_package().unwrap();
        client
            .set_external_sender(server.external_sender_package())
            .unwrap();
        let proposals = server.on_key_package(&kp).unwrap();
        let ops = match proposals[0] {
            0 => davey::ProposalsOperationType::APPEND,
            _ => davey::ProposalsOperationType::REVOKE,
        };
        let cw = client
            .process_proposals(ops, &proposals[1..], Some(&[9999]))
            .unwrap()
            .unwrap();
        let mut payload = cw.commit.clone();
        payload.extend(cw.welcome.unwrap());
        let (tid, announce) = server.on_commit_welcome(&payload).unwrap();
        assert_eq!(tid, 1);
        client.process_commit(&announce[2..]).unwrap();
        assert!(client.is_ready());
        assert!(server.is_active());
        assert_eq!(
            client.voice_privacy_code(),
            server.privacy_code().as_deref()
        );

        let frame = [0, 0, 0, 1, 0x65, 0x88, 0x84, 0x21, 0x10, 0x20];
        let enc = client
            .encrypt(MediaType::VIDEO, Codec::H264, &frame)
            .unwrap()
            .into_owned();
        assert!(is_dave_frame(&enc));
        assert_eq!(server.decrypt(MediaType::VIDEO, &enc).unwrap(), frame);
    }
}
