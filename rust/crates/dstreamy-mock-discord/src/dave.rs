//! Server side of DAVE: Discord's MLS external sender plus a simulated participant the client adds.

use std::num::NonZeroU16;
use std::time::{Duration, Instant};

use davey::{DaveSession, MediaType, ProposalsOperationType};
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
    #[error("davey: {0}")]
    Decrypt(#[from] davey::errors::DecryptError),
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
    /// A second simulated member, added and removed by the re-key hooks.
    guest: Option<DaveSession>,
    next_transition: u16,
    /// `None` until a transition executes: plaintext is accepted without limit.
    pub(crate) plaintext_until: Option<Instant>,
    /// After a transition to protocol 0 the client sends plaintext for good.
    pub(crate) downgraded: bool,
    /// Bumped for every new participant session and epoch; davey's decryptor starts each without a nonce baseline.
    key_generation: u64,
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
            guest: None,
            next_transition: 0,
            plaintext_until: None,
            downgraded: false,
            key_generation: 0,
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
        // The client starts a fresh group with every key package, so the member and the epoch start over.
        self.guest = None;
        let (participant, proposal) =
            self.member_add_proposal(self.participant_id, GroupEpoch::from(0))?;
        self.participant = Some(participant);
        self.key_generation += 1;
        Ok(proposal)
    }

    /// An external Add proposal for a second member (`user_id`), as when someone else joins the call.
    pub fn add_guest_proposal(&mut self, user_id: u64) -> Result<Vec<u8>, DaveServerError> {
        if !self.is_active() {
            return Err(DaveServerError::NoParticipant);
        }
        let (guest, proposal) = self.member_add_proposal(user_id, self.epoch())?;
        self.queue_in_members(ProposalsOperationType::APPEND, &proposal[1..])?;
        self.guest = Some(guest);
        Ok(proposal)
    }

    /// Every member sees the proposals and keeps its own pending commit until the announced one lands, as on Discord.
    fn queue_in_members(
        &mut self,
        operation: ProposalsOperationType,
        proposals: &[u8],
    ) -> Result<(), DaveServerError> {
        for member in [&mut self.participant, &mut self.guest]
            .into_iter()
            .flatten()
            .filter(|m| m.is_ready())
        {
            member
                .process_proposals(operation, proposals, None)
                .map_err(davey)?;
        }
        Ok(())
    }

    /// An external Remove proposal for the guest, as when that member leaves the call.
    pub fn remove_guest_proposal(&mut self) -> Result<Vec<u8>, DaveServerError> {
        let leaf = self
            .guest
            .as_ref()
            .filter(|g| g.is_ready())
            .and_then(DaveSession::own_leaf_index)
            .ok_or(DaveServerError::NoParticipant)?;
        let proposal = ExternalProposal::new_remove::<OpenMlsRustCrypto>(
            leaf,
            GroupId::from_slice(&self.channel_id.to_be_bytes()),
            self.epoch(),
            &self.signer,
            SenderExtensionIndex::new(0),
        )
        .map_err(mls)?
        .tls_serialize_detached()
        .map_err(mls)?;
        // DAVE's REVOKE withdraws earlier proposals; a member leaving is an appended MLS Remove.
        let payload = proposals_payload(0, proposal)?;
        self.guest = None;
        self.queue_in_members(ProposalsOperationType::APPEND, &payload[1..])?;
        Ok(payload)
    }

    fn member_add_proposal(
        &self,
        user_id: u64,
        epoch: GroupEpoch,
    ) -> Result<(DaveSession, Vec<u8>), DaveServerError> {
        let mut member = DaveSession::new(self.protocol_version, user_id, self.channel_id, None)
            .map_err(davey)?;
        member
            .set_external_sender(&self.external_sender)
            .map_err(davey)?;
        let member_kp = member.create_key_package().map_err(davey)?;
        let member_kp = parse_key_package(&self.provider, &member_kp)?;
        let proposal = ExternalProposal::new_add::<OpenMlsRustCrypto>(
            member_kp,
            GroupId::from_slice(&self.channel_id.to_be_bytes()),
            epoch,
            &self.signer,
            SenderExtensionIndex::new(0),
        )
        .map_err(mls)?
        .tls_serialize_detached()
        .map_err(mls)?;
        Ok((member, proposals_payload(0, proposal)?))
    }

    fn epoch(&self) -> GroupEpoch {
        self.participant
            .as_ref()
            .and_then(DaveSession::epoch)
            .unwrap_or_else(|| GroupEpoch::from(0))
    }

    /// Members in the group apply the commit; a member still joining takes the welcome.
    pub fn on_commit_welcome(&mut self, payload: &[u8]) -> Result<(u16, Vec<u8>), DaveServerError> {
        let (_, rest) = MlsMessageIn::tls_deserialize_bytes(payload).map_err(mls)?;
        let commit = &payload[..payload.len() - rest.len()];
        let welcome = (!rest.is_empty()).then_some(rest);
        if self.participant.is_none() {
            return Err(DaveServerError::NoParticipant);
        }
        for member in [&mut self.participant, &mut self.guest]
            .into_iter()
            .flatten()
        {
            if member.is_ready() {
                member.process_commit(commit).map_err(davey)?;
            } else if let Some(welcome) = welcome {
                member.process_welcome(welcome).map_err(davey)?;
            }
        }
        self.key_generation += 1;
        self.next_transition = self.next_transition.wrapping_add(1).max(1);
        let transition_id = self.next_transition;
        let mut out = transition_id.to_be_bytes().to_vec();
        out.extend_from_slice(commit);
        Ok((transition_id, out))
    }

    /// The id the next PREPARE_TRANSITION announces.
    pub fn next_transition_id(&mut self) -> u16 {
        self.next_transition = self.next_transition.wrapping_add(1).max(1);
        self.next_transition
    }

    /// Forgets the group ahead of a new epoch; the client's next key package starts it over.
    pub fn reset_group(&mut self) {
        self.participant = None;
        self.guest = None;
        self.plaintext_until = None;
        self.downgraded = false;
    }

    pub fn execute_transition(&mut self) {
        let until = Instant::now() + TRANSITION_EXPIRY;
        self.plaintext_until = Some(self.plaintext_until.map_or(until, |t| t.min(until)));
    }

    pub fn accepts_plaintext(&self) -> bool {
        self.downgraded || self.plaintext_until.is_none_or(|t| Instant::now() < t)
    }

    pub fn has_guest(&self) -> bool {
        self.guest.as_ref().is_some_and(DaveSession::is_ready)
    }

    pub fn key_generation(&self) -> u64 {
        self.key_generation
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
        Ok(participant.decrypt(self.client_user_id, media_type, frame)?)
    }
}

fn proposals_payload(op_type: u8, proposal: Vec<u8>) -> Result<Vec<u8>, DaveServerError> {
    let mut payload = vec![op_type];
    payload.extend(
        VLBytes::new(proposal)
            .tls_serialize_detached()
            .map_err(mls)?,
    );
    Ok(payload)
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

        let commit_proposals = |client: &mut DaveSession, proposals: Vec<u8>| {
            let ops = match proposals[0] {
                0 => davey::ProposalsOperationType::APPEND,
                _ => davey::ProposalsOperationType::REVOKE,
            };
            let cw = client
                .process_proposals(ops, &proposals[1..], Some(&[9999, 8888]))
                .unwrap()
                .unwrap();
            let mut payload = cw.commit.clone();
            if let Some(w) = cw.welcome {
                payload.extend(w);
            }
            payload
        };
        let epoch_before = client.epoch().unwrap();
        let joined = commit_proposals(&mut client, server.add_guest_proposal(8888).unwrap());
        let (tid, announce) = server.on_commit_welcome(&joined).unwrap();
        assert_eq!(tid, 2);
        client.process_commit(&announce[2..]).unwrap();
        assert!(server.has_guest());
        assert!(client.epoch().unwrap() > epoch_before);
        let enc = client
            .encrypt(MediaType::VIDEO, Codec::H264, &frame)
            .unwrap()
            .into_owned();
        assert_eq!(server.decrypt(MediaType::VIDEO, &enc).unwrap(), frame);

        let left = commit_proposals(&mut client, server.remove_guest_proposal().unwrap());
        let (tid, announce) = server.on_commit_welcome(&left).unwrap();
        assert_eq!(tid, 3);
        client.process_commit(&announce[2..]).unwrap();
        assert!(!server.has_guest());
        let enc = client
            .encrypt(MediaType::VIDEO, Codec::H264, &frame)
            .unwrap()
            .into_owned();
        assert_eq!(server.decrypt(MediaType::VIDEO, &enc).unwrap(), frame);
    }
}
