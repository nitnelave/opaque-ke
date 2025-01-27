// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! Provides the main OPAQUE API

use crate::{
    ciphersuite::CipherSuite,
    envelope::{mode_from_ids, Envelope},
    errors::{utils::check_slice_size_atleast, InternalPakeError, PakeError, ProtocolError},
    group::Group,
    hash::Hash,
    key_exchange::traits::{FromBytes, KeyExchange, ToBytesWithPointers},
    keypair::{KeyPair, PrivateKey, PublicKey, SizedBytesExt},
    map_to_curve::GroupWithMapToCurve,
    oprf,
    serialization::{serialize, tokenize},
    slow_hash::SlowHash,
    CredentialFinalization, CredentialRequest, CredentialResponse, RegistrationRequest,
    RegistrationResponse, RegistrationUpload,
};
use digest::Digest;
use generic_array::{typenum::Unsigned, GenericArray};
use generic_bytes::SizedBytes;
use rand::{CryptoRng, RngCore};
use std::marker::PhantomData;
use zeroize::Zeroize;

// Registration
// ============

/// The state elements the client holds to register itself
pub struct ClientRegistration<CS: CipherSuite> {
    /// token containing the client's password and the blinding factor
    pub(crate) token: oprf::Token<CS::Group>,
}

// Cannot be derived because it would require for CS to be Clone.
impl<CS: CipherSuite> Clone for ClientRegistration<CS> {
    fn clone(&self) -> Self {
        Self {
            token: self.token.clone(),
        }
    }
}

impl<CS: CipherSuite> ClientRegistration<CS> {
    /// Serialization into bytes
    pub fn serialize(&self) -> Vec<u8> {
        let output: Vec<u8> = [
            &CS::Group::scalar_as_bytes(&self.token.blind)[..],
            &self.token.data,
        ]
        .concat();
        output
    }

    /// Deserialization from bytes
    pub fn deserialize(input: &[u8]) -> Result<Self, ProtocolError> {
        let min_expected_len = <CS::Group as Group>::ScalarLen::to_usize();
        let checked_slice = (if input.len() <= min_expected_len {
            Err(InternalPakeError::SizeError {
                name: "client_registration_bytes",
                len: min_expected_len,
                actual_len: input.len(),
            })
        } else {
            Ok(input)
        })?;

        // Check that the message is actually containing an element of the
        // correct subgroup
        let scalar_len = min_expected_len;
        let blinding_factor_bytes = GenericArray::from_slice(&checked_slice[..scalar_len]);
        let blinding_factor = CS::Group::from_scalar_slice(blinding_factor_bytes)?;
        let password = checked_slice[scalar_len..].to_vec();
        Ok(Self {
            token: oprf::Token {
                data: password,
                blind: blinding_factor,
            },
        })
    }

    #[cfg(test)]
    pub fn as_byte_ptrs(&self) -> Vec<(*const u8, usize)> {
        vec![
            (self.token.data.as_ptr(), self.token.data.len()),
            /* cannot provide raw pointer to self.token.blind until this is exposed in curve25519_dalek::scalar::Scalar */
        ]
    }
}

impl_serialize_and_deserialize_for!(ClientRegistration);

/// Optional parameters for client registration finish
#[derive(Clone)]
pub enum ClientRegistrationFinishParameters {
    /// Specifying the identifiers idU and idS (corresponding to custom identifier mode)
    WithIdentifiers(Vec<u8>, Vec<u8>),
    /// No identifiers specified (corresponding to base mode)
    Default,
}

impl Default for ClientRegistrationFinishParameters {
    fn default() -> Self {
        Self::Default
    }
}

/// Contains the fields that are returned by a client registration start
pub struct ClientRegistrationStartResult<CS: CipherSuite> {
    /// The registration request message to be sent to the server
    pub message: RegistrationRequest<CS>,
    /// The client state that must be persisted in order to complete registration
    pub state: ClientRegistration<CS>,
}

// Cannot be derived because it would require for CS to be Clone.
impl<CS: CipherSuite> Clone for ClientRegistrationStartResult<CS> {
    fn clone(&self) -> Self {
        Self {
            message: self.message.clone(),
            state: self.state.clone(),
        }
    }
}

impl<CS: CipherSuite> ClientRegistration<CS> {
    /// Returns an initial "blinded" request to send to the server, as well as a ClientRegistration
    ///
    /// # Arguments
    /// * `password` - A user password
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::ClientRegistration;
    /// # use opaque_ke::errors::ProtocolError;
    /// use rand::{rngs::OsRng, RngCore};
    /// use opaque_ke::ciphersuite::CipherSuite;
    /// struct Default;
    /// impl CipherSuite for Default {
    ///     type Group = curve25519_dalek::ristretto::RistrettoPoint;
    ///     type KeyExchange = opaque_ke::key_exchange::tripledh::TripleDH;
    ///     type Hash = sha2::Sha512;
    ///     type SlowHash = opaque_ke::slow_hash::NoOpHash;
    /// }
    /// let mut client_rng = OsRng;
    /// let client_registration_start_result = ClientRegistration::<Default>::start(&mut client_rng, b"hunter2")?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn start<R: RngCore + CryptoRng>(
        blinding_factor_rng: &mut R,
        password: &[u8],
    ) -> Result<ClientRegistrationStartResult<CS>, ProtocolError> {
        let (token, alpha) = oprf::blind::<R, CS::Group, CS::Hash>(password, blinding_factor_rng)?;

        Ok(ClientRegistrationStartResult {
            message: RegistrationRequest::<CS> { alpha },
            state: Self { token },
        })
    }
}

/// Contains the fields that are returned by a client registration finish
pub struct ClientRegistrationFinishResult<CS: CipherSuite> {
    /// The registration upload message to be sent to the server
    pub message: RegistrationUpload<CS>,
    /// The export key output by client registration
    pub export_key: GenericArray<u8, <CS::Hash as Digest>::OutputSize>,
    /// Instance of the ClientRegistration, only used in tests for checking zeroize
    #[cfg(test)]
    pub state: ClientRegistration<CS>,
}

// Cannot be derived because it would require for CS to be Clone.
impl<CS: CipherSuite> Clone for ClientRegistrationFinishResult<CS> {
    fn clone(&self) -> Self {
        Self {
            message: self.message.clone(),
            export_key: self.export_key.clone(),
            #[cfg(test)]
            state: self.state.clone(),
        }
    }
}

impl<CS: CipherSuite> ClientRegistration<CS> {
    /// "Unblinds" the server's answer and returns a final message containing
    /// cryptographic identifiers, to be sent to the server on setup finalization
    ///
    /// # Arguments
    /// * `message` - the server's answer to the initial registration attempt
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::{ClientRegistration, ClientRegistrationFinishParameters, ServerRegistration};
    /// # use opaque_ke::errors::ProtocolError;
    /// # use opaque_ke::keypair::KeyPair;
    /// use rand::{rngs::OsRng, RngCore};
    /// use opaque_ke::ciphersuite::CipherSuite;
    /// struct Default;
    /// impl CipherSuite for Default {
    ///     type Group = curve25519_dalek::ristretto::RistrettoPoint;
    ///     type KeyExchange = opaque_ke::key_exchange::tripledh::TripleDH;
    ///     type Hash = sha2::Sha512;
    ///     type SlowHash = opaque_ke::slow_hash::NoOpHash;
    /// }
    /// let mut client_rng = OsRng;
    /// let mut server_rng = OsRng;
    /// let server_kp = Default::generate_random_keypair(&mut server_rng);
    /// let client_registration_start_result = ClientRegistration::<Default>::start(&mut client_rng, b"hunter2")?;
    /// let server_registration_start_result =
    /// ServerRegistration::<Default>::start(&mut server_rng, client_registration_start_result.message, server_kp.public())?;
    /// let mut client_rng = OsRng;
    /// let client_registration_finish_result = client_registration_start_result.state.finish(&mut client_rng, server_registration_start_result.message, ClientRegistrationFinishParameters::default())?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn finish<R: CryptoRng + RngCore>(
        self,
        rng: &mut R,
        r2: RegistrationResponse<CS>,
        params: ClientRegistrationFinishParameters,
    ) -> Result<ClientRegistrationFinishResult<CS>, ProtocolError> {
        let optional_ids = match params {
            ClientRegistrationFinishParameters::WithIdentifiers(id_u, id_s) => Some((id_u, id_s)),
            ClientRegistrationFinishParameters::Default => None,
        };
        let client_static_keypair = CS::generate_random_keypair(rng);

        let password_derived_key =
            get_password_derived_key::<CS::Group, CS::SlowHash, CS::Hash>(&self.token, r2.beta)?;

        let (envelope, export_key) = Envelope::<CS::Hash>::seal(
            rng,
            &password_derived_key,
            &client_static_keypair.private().to_arr().to_vec(),
            &r2.server_s_pk,
            optional_ids,
        )?;

        Ok(ClientRegistrationFinishResult {
            message: RegistrationUpload {
                envelope,
                client_s_pk: client_static_keypair.public().clone(),
            },
            export_key,
            #[cfg(test)]
            state: self,
        })
    }
}

/// Contains the fields that are returned by a server registration start
pub struct ServerRegistrationStartResult<CS: CipherSuite> {
    /// The registration resposne message to send to the client
    pub message: RegistrationResponse<CS>,
    /// The state that the server must keep in order to complete registration
    pub state: ServerRegistration<CS>,
}

// Cannot be derived because it would require for CS to be Clone.
impl<CS: CipherSuite> Clone for ServerRegistrationStartResult<CS> {
    fn clone(&self) -> Self {
        Self {
            message: self.message.clone(),
            state: self.state.clone(),
        }
    }
}

/// The state elements the server holds to record a registration
pub struct ServerRegistration<CS: CipherSuite> {
    envelope: Option<Envelope<CS::Hash>>,
    client_s_pk: Option<PublicKey>,
    pub(crate) oprf_key: <CS::Group as Group>::Scalar,
}

// Cannot be derived because it would require for CS to be Clone.
impl<CS: CipherSuite> Clone for ServerRegistration<CS> {
    fn clone(&self) -> Self {
        Self {
            envelope: self.envelope.clone(),
            client_s_pk: self.client_s_pk.clone(),
            oprf_key: self.oprf_key.clone(),
        }
    }
}

impl<CS: CipherSuite> ServerRegistration<CS> {
    /// Serialization into bytes
    pub fn serialize(&self) -> Vec<u8> {
        let mut output: Vec<u8> = CS::Group::scalar_as_bytes(&self.oprf_key).to_vec();
        self.client_s_pk
            .iter()
            .for_each(|v| output.extend_from_slice(&v.to_arr()));
        self.envelope
            .iter()
            .for_each(|v| output.extend_from_slice(&v.to_bytes()));
        output
    }

    /// Deserialization from bytes
    pub fn deserialize(input: &[u8]) -> Result<Self, ProtocolError> {
        let scalar_len = <CS::Group as Group>::ScalarLen::to_usize();
        if input.len() == scalar_len {
            return Ok(Self {
                oprf_key: CS::Group::from_scalar_slice(GenericArray::from_slice(input))?,
                client_s_pk: None,
                envelope: None,
            });
        }

        // Need to do this check manually because envelope is variable-size
        let key_len = <PublicKey as SizedBytes>::Len::to_usize();

        let checked_bytes =
            check_slice_size_atleast(input, scalar_len + key_len, "server_registration_bytes")?;

        let oprf_key_bytes = GenericArray::from_slice(&checked_bytes[..scalar_len]);
        let oprf_key = CS::Group::from_scalar_slice(oprf_key_bytes)?;
        let unchecked_client_s_pk =
            PublicKey::from_bytes(&checked_bytes[scalar_len..scalar_len + key_len])?;
        let client_s_pk = KeyPair::<CS::Group>::check_public_key(unchecked_client_s_pk)?;

        let envelope = Envelope::<CS::Hash>::from_bytes(&checked_bytes[scalar_len + key_len..])?;

        Ok(Self {
            envelope: Some(envelope),
            client_s_pk: Some(client_s_pk),
            oprf_key,
        })
    }

    #[cfg(test)]
    pub fn as_byte_ptrs(&self) -> Vec<(*const u8, usize)> {
        [
            match &self.envelope {
                Some(env) => env.as_byte_ptrs(),
                None => vec![],
            },
            match &self.client_s_pk {
                Some(pk) => vec![(pk.as_ptr(), pk.len())],
                None => vec![],
            },
            /* cannot provide raw pointer to self.oprf_key until this is exposed in curve25519_dalek::scalar::Scalar */
        ].concat()
    }

    /// From the client's "blinded" password, returns a response to be
    /// sent back to the client, as well as a ServerRegistration
    ///
    /// # Arguments
    /// * `message`   - the initial registration message
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::*;
    /// # use opaque_ke::errors::ProtocolError;
    /// use rand::{rngs::OsRng, RngCore};
    /// use opaque_ke::ciphersuite::CipherSuite;
    /// struct Default;
    /// impl CipherSuite for Default {
    ///     type Group = curve25519_dalek::ristretto::RistrettoPoint;
    ///     type KeyExchange = opaque_ke::key_exchange::tripledh::TripleDH;
    ///     type Hash = sha2::Sha512;
    ///     type SlowHash = opaque_ke::slow_hash::NoOpHash;
    /// }
    /// let mut client_rng = OsRng;
    /// let mut server_rng = OsRng;
    /// let server_kp = Default::generate_random_keypair(&mut server_rng);
    /// let client_registration_start_result = ClientRegistration::<Default>::start(&mut client_rng, b"hunter2")?;
    /// let server_registration_start_result = ServerRegistration::<Default>::start(&mut server_rng, client_registration_start_result.message, server_kp.public())?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn start<R: RngCore + CryptoRng>(
        rng: &mut R,
        message: RegistrationRequest<CS>,
        server_s_pk: &PublicKey,
    ) -> Result<ServerRegistrationStartResult<CS>, ProtocolError> {
        // RFC: generate oprf_key (salt) and v_u = g^oprf_key
        let oprf_key = CS::Group::random_scalar(rng);

        // Compute beta = alpha^oprf_key
        let beta = oprf::evaluate::<CS::Group>(message.alpha, &oprf_key);

        Ok(ServerRegistrationStartResult {
            message: RegistrationResponse {
                beta,
                server_s_pk: server_s_pk.clone(),
            },
            state: Self {
                envelope: None,
                client_s_pk: None,
                oprf_key,
            },
        })
    }

    /// From the client's cryptographic identifiers, fully populates and
    /// returns a ServerRegistration
    ///
    /// # Arguments
    /// * `message` - the final client message
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::{*, keypair::KeyPair};
    /// # use opaque_ke::errors::ProtocolError;
    /// use rand::{rngs::OsRng, RngCore};
    /// use opaque_ke::ciphersuite::CipherSuite;
    /// struct Default;
    /// impl CipherSuite for Default {
    ///     type Group = curve25519_dalek::ristretto::RistrettoPoint;
    ///     type KeyExchange = opaque_ke::key_exchange::tripledh::TripleDH;
    ///     type Hash = sha2::Sha512;
    ///     type SlowHash = opaque_ke::slow_hash::NoOpHash;
    /// }
    /// let mut client_rng = OsRng;
    /// let mut server_rng = OsRng;
    /// let server_kp = Default::generate_random_keypair(&mut server_rng);
    /// let client_registration_start_result = ClientRegistration::<Default>::start(&mut client_rng, b"hunter2")?;
    /// let server_registration_start_result = ServerRegistration::<Default>::start(&mut server_rng, client_registration_start_result.message, server_kp.public())?;
    /// let mut client_rng = OsRng;
    /// let client_registration_finish_result = client_registration_start_result.state.finish(&mut client_rng, server_registration_start_result.message, ClientRegistrationFinishParameters::default())?;
    /// let client_record = server_registration_start_result.state.finish(client_registration_finish_result.message)?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn finish(self, message: RegistrationUpload<CS>) -> Result<Self, ProtocolError> {
        Ok(Self {
            envelope: Some(message.envelope),
            client_s_pk: Some(message.client_s_pk),
            oprf_key: self.oprf_key.clone(),
        })
    }
}

impl_serialize_and_deserialize_for!(ServerRegistration);

// Login
// =====

/// The state elements the client holds to perform a login
pub struct ClientLogin<CS: CipherSuite> {
    /// token containing the client's password and the blinding factor
    token: oprf::Token<CS::Group>,
    ke1_state: <CS::KeyExchange as KeyExchange<CS::Hash, CS::Group>>::KE1State,
    serialized_credential_request: Vec<u8>,
}

// Cannot be derived because it would require for CS to be Clone.
impl<CS: CipherSuite> Clone for ClientLogin<CS> {
    fn clone(&self) -> Self {
        Self {
            token: self.token.clone(),
            ke1_state: self.ke1_state.clone(),
            serialized_credential_request: self.serialized_credential_request.clone(),
        }
    }
}

impl<CS: CipherSuite> ClientLogin<CS> {
    /// Serialization into bytes
    pub fn serialize(&self) -> Vec<u8> {
        let output: Vec<u8> = [
            &CS::Group::scalar_as_bytes(&self.token.blind)[..],
            &serialize(&self.serialized_credential_request, 2),
            &serialize(&self.ke1_state.to_bytes(), 2),
            &self.token.data,
        ]
        .concat();
        output
    }

    /// Deserialization from bytes
    pub fn deserialize(input: &[u8]) -> Result<Self, ProtocolError> {
        let scalar_len = <CS::Group as Group>::ScalarLen::to_usize();
        let checked_slice = (if input.len() <= scalar_len {
            Err(InternalPakeError::SizeError {
                name: "client_login_bytes",
                len: scalar_len,
                actual_len: input.len(),
            })
        } else {
            Ok(input)
        })?;

        let blinding_factor_bytes = GenericArray::from_slice(&checked_slice[..scalar_len]);
        let blinding_factor = CS::Group::from_scalar_slice(blinding_factor_bytes)?;

        let (serialized_credential_request, remainder) = tokenize(&checked_slice[scalar_len..], 2)?;
        let (ke1_state_bytes, password) = tokenize(&remainder, 2)?;

        let ke1_state =
            <CS::KeyExchange as KeyExchange<CS::Hash, CS::Group>>::KE1State::from_bytes::<CS>(
                &ke1_state_bytes[..],
            )?;
        Ok(Self {
            token: oprf::Token {
                data: password,
                blind: blinding_factor,
            },
            ke1_state,
            serialized_credential_request,
        })
    }

    #[cfg(test)]
    pub fn as_byte_ptrs(&self) -> Vec<(*const u8, usize)> {
        [
            vec![
                (self.token.data.as_ptr(), self.token.data.len()),
                /* cannot provide raw pointer to self.token.blind until this is exposed in curve25519_dalek::scalar::Scalar */
            ],
            self.ke1_state.as_byte_ptrs(),
            vec![ (self.serialized_credential_request.as_ptr(), self.serialized_credential_request.len()) ],
        ].concat()
    }
}

impl_serialize_and_deserialize_for!(ClientLogin);

/// Optional parameters for client login start
#[derive(Clone)]
pub enum ClientLoginStartParameters {
    /// Specifying a plaintext info field that will be sent to the server
    WithInfo(Vec<u8>),
}

impl Default for ClientLoginStartParameters {
    fn default() -> Self {
        Self::WithInfo(Vec::new())
    }
}

/// Contains the fields that are returned by a client login start
pub struct ClientLoginStartResult<CS: CipherSuite> {
    /// The message to send to the server to begin the login protocol
    pub message: CredentialRequest<CS>,
    /// The state that the client must keep in order to complete the protocol
    pub state: ClientLogin<CS>,
}

// Cannot be derived because it would require for CS to be Clone.
impl<CS: CipherSuite> Clone for ClientLoginStartResult<CS> {
    fn clone(&self) -> Self {
        Self {
            message: self.message.clone(),
            state: self.state.clone(),
        }
    }
}

/// Optional parameters for client login finish
#[derive(Clone)]
pub enum ClientLoginFinishParameters {
    /// Specifying a user identifier and server identifier that will be matched against the client
    WithIdentifiers(Vec<u8>, Vec<u8>),
    /// No info and no custom identifiers
    Default,
}

impl Default for ClientLoginFinishParameters {
    fn default() -> Self {
        Self::Default
    }
}

/// Contains the fields that are returned by a client login finish
pub struct ClientLoginFinishResult<CS: CipherSuite> {
    /// The message to send to the server to complete the protocol
    pub message: CredentialFinalization<CS>,
    /// The session key
    pub session_key: Vec<u8>,
    /// The client-side export key
    pub export_key: GenericArray<u8, <CS::Hash as Digest>::OutputSize>,
    /// The server's static public key
    pub server_s_pk: PublicKey,
    /// The confidential info sent by the client
    pub confidential_info: Vec<u8>,
    /// Instance of the ClientLogin, only used in tests for checking zeroize
    #[cfg(test)]
    pub state: ClientLogin<CS>,
}

// Cannot be derived because it would require for CS to be Clone.
impl<CS: CipherSuite> Clone for ClientLoginFinishResult<CS> {
    fn clone(&self) -> Self {
        Self {
            message: self.message.clone(),
            session_key: self.session_key.clone(),
            export_key: self.export_key.clone(),
            server_s_pk: self.server_s_pk.clone(),
            confidential_info: self.confidential_info.clone(),
            #[cfg(test)]
            state: self.state.clone(),
        }
    }
}

impl<CS: CipherSuite> ClientLogin<CS> {
    /// Returns an initial "blinded" password request to send to the server, as well as a ClientLogin
    ///
    /// # Arguments
    /// * `password` - A user password
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::{ClientLogin, ClientLoginStartParameters};
    /// # use opaque_ke::errors::ProtocolError;
    /// use rand::{rngs::OsRng, RngCore};
    /// use opaque_ke::ciphersuite::CipherSuite;
    /// struct Default;
    /// impl CipherSuite for Default {
    ///     type Group = curve25519_dalek::ristretto::RistrettoPoint;
    ///     type KeyExchange = opaque_ke::key_exchange::tripledh::TripleDH;
    ///     type Hash = sha2::Sha512;
    ///     type SlowHash = opaque_ke::slow_hash::NoOpHash;
    /// }
    /// let mut client_rng = OsRng;
    /// let client_login_start_result = ClientLogin::<Default>::start(&mut client_rng, b"hunter2", ClientLoginStartParameters::default())?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn start<R: RngCore + CryptoRng>(
        rng: &mut R,
        password: &[u8],
        params: ClientLoginStartParameters,
    ) -> Result<ClientLoginStartResult<CS>, ProtocolError> {
        let ClientLoginStartParameters::WithInfo(info) = params;

        let (token, alpha) = oprf::blind::<R, CS::Group, CS::Hash>(password, rng)?;

        let (ke1_state, ke1_message) = CS::KeyExchange::generate_ke1(info, rng)?;

        let credential_request = CredentialRequest { alpha, ke1_message };
        let serialized_credential_request = credential_request.serialize();

        Ok(ClientLoginStartResult {
            message: credential_request,
            state: Self {
                token,
                ke1_state,
                serialized_credential_request,
            },
        })
    }

    /// "Unblinds" the server's answer and returns the opened assets from
    /// the server
    ///
    /// # Arguments
    /// * `message` - the server's answer to the initial login attempt
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::{ClientLogin, ClientLoginStartParameters, ClientLoginFinishParameters, ServerLogin, ServerLoginStartParameters};
    /// # use opaque_ke::{ClientRegistration, ClientRegistrationFinishParameters, ServerRegistration};
    /// # use opaque_ke::errors::ProtocolError;
    /// # use opaque_ke::keypair::KeyPair;
    /// use rand::{rngs::OsRng, RngCore};
    /// use opaque_ke::ciphersuite::CipherSuite;
    /// struct Default;
    /// impl CipherSuite for Default {
    ///     type Group = curve25519_dalek::ristretto::RistrettoPoint;
    ///     type KeyExchange = opaque_ke::key_exchange::tripledh::TripleDH;
    ///     type Hash = sha2::Sha512;
    ///     type SlowHash = opaque_ke::slow_hash::NoOpHash;
    /// }
    /// let mut client_rng = OsRng;
    /// # let mut server_rng = OsRng;
    /// # let client_registration_start_result = ClientRegistration::<Default>::start(&mut client_rng, b"hunter2")?;
    /// # let server_kp = Default::generate_random_keypair(&mut server_rng);
    /// # let server_registration_start_result = ServerRegistration::<Default>::start(&mut server_rng, client_registration_start_result.message, server_kp.public())?;
    /// # let client_registration_finish_result = client_registration_start_result.state.finish(&mut client_rng, server_registration_start_result.message, ClientRegistrationFinishParameters::default())?;
    /// # let p_file = server_registration_start_result.state.finish(client_registration_finish_result.message)?;
    /// let client_login_start_result = ClientLogin::<Default>::start(&mut client_rng, b"hunter2", ClientLoginStartParameters::default())?;
    /// let server_login_start_result = ServerLogin::start(&mut server_rng, p_file, &server_kp.private(), client_login_start_result.message, ServerLoginStartParameters::default())?;
    /// let client_login_finish_result = client_login_start_result.state.finish(server_login_start_result.message, ClientLoginFinishParameters::default())?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn finish(
        self,
        l2: CredentialResponse<CS>,
        params: ClientLoginFinishParameters,
    ) -> Result<ClientLoginFinishResult<CS>, ProtocolError> {
        let optional_ids = match params {
            ClientLoginFinishParameters::Default => None,
            ClientLoginFinishParameters::WithIdentifiers(id_u, id_s) => Some((id_u, id_s)),
        };

        let server_s_pk_bytes = l2.server_s_pk.to_arr().to_vec();

        let password_derived_key =
            get_password_derived_key::<CS::Group, CS::SlowHash, CS::Hash>(&self.token, l2.beta)?;
        let opened_envelope = &l2
            .envelope
            .open(&password_derived_key, &server_s_pk_bytes, &optional_ids)
            .map_err(|e| match e {
                InternalPakeError::SealOpenHmacError => PakeError::InvalidLoginError,
                err => PakeError::from(err),
            })?;

        let client_s_sk = PrivateKey::from_bytes(&opened_envelope.client_s_sk)?;

        let (id_u, id_s) = match optional_ids {
            None => (
                KeyPair::<CS::Group>::public_from_private(&client_s_sk)
                    .to_arr()
                    .to_vec(),
                server_s_pk_bytes,
            ),
            Some((id_u, id_s)) => (id_u, id_s),
        };

        let credential_response_component =
            CredentialResponse::<CS>::serialize_without_ke(&l2.beta, &l2.server_s_pk, &l2.envelope);

        let (confidential_info, session_key, ke3_message) = CS::KeyExchange::generate_ke3(
            credential_response_component,
            l2.ke2_message,
            &self.ke1_state,
            &self.serialized_credential_request,
            l2.server_s_pk.clone(),
            client_s_sk,
            id_u,
            id_s,
        )?;

        Ok(ClientLoginFinishResult {
            confidential_info,
            message: CredentialFinalization { ke3_message },
            session_key,
            export_key: opened_envelope.export_key.clone(),
            server_s_pk: l2.server_s_pk,
            #[cfg(test)]
            state: self,
        })
    }
}

/// The state elements the server holds to record a login
pub struct ServerLogin<CS: CipherSuite> {
    ke2_state: <CS::KeyExchange as KeyExchange<CS::Hash, CS::Group>>::KE2State,
    _cs: PhantomData<CS>,
}

// Cannot be derived because it would require for CS to be Clone.
impl<CS: CipherSuite> Clone for ServerLogin<CS> {
    fn clone(&self) -> Self {
        Self {
            ke2_state: self.ke2_state.clone(),
            _cs: PhantomData,
        }
    }
}

/// Optional parameters for server login start
#[derive(Clone)]
pub enum ServerLoginStartParameters {
    /// Specifying a confidential info field that will be sent to the client
    WithInfo(Vec<u8>),
    /// Specifying a user identifier and server identifier that will be matched against the client
    WithIdentifiers(Vec<u8>, Vec<u8>),
    /// Specifying a confidential info field that will be sent to the client,
    /// along with a user identifier and and server identifier that will be matched against the client
    /// (in that order)
    WithInfoAndIdentifiers(Vec<u8>, Vec<u8>, Vec<u8>),
}

impl Default for ServerLoginStartParameters {
    fn default() -> Self {
        Self::WithInfo(Vec::new())
    }
}

/// Contains the fields that are returned by a server login start
pub struct ServerLoginStartResult<CS: CipherSuite> {
    /// The message to send back to the client
    pub message: CredentialResponse<CS>,
    /// The state that the server must keep in order to finish the protocl
    pub state: ServerLogin<CS>,
    /// The plaintext info sent by the client
    pub plain_info: Vec<u8>,
}

// Cannot be derived because it would require for CS to be Clone.
impl<CS: CipherSuite> Clone for ServerLoginStartResult<CS> {
    fn clone(&self) -> Self {
        Self {
            message: self.message.clone(),
            state: self.state.clone(),
            plain_info: self.plain_info.clone(),
        }
    }
}

/// Contains the fields that are returned by a server login finish
pub struct ServerLoginFinishResult<CS: CipherSuite> {
    /// The session key between client and server
    pub session_key: Vec<u8>,
    _cs: PhantomData<CS>,
    /// Instance of the ClientRegistration, only used in tests for checking zeroize
    #[cfg(test)]
    pub state: ServerLogin<CS>,
}

// Cannot be derived because it would require for CS to be Clone.
impl<CS: CipherSuite> Clone for ServerLoginFinishResult<CS> {
    fn clone(&self) -> Self {
        Self {
            session_key: self.session_key.clone(),
            _cs: PhantomData,
            #[cfg(test)]
            state: self.state.clone(),
        }
    }
}

impl<CS: CipherSuite> ServerLogin<CS> {
    /// Serialization into bytes
    pub fn serialize(&self) -> Vec<u8> {
        self.ke2_state.to_bytes()
    }

    /// Deserialization from bytes
    pub fn deserialize(bytes: &[u8]) -> Result<Self, ProtocolError> {
        Ok(Self {
            _cs: PhantomData,
            ke2_state: <CS::KeyExchange as KeyExchange<CS::Hash, CS::Group>>::KE2State::from_bytes::<
                CS,
            >(bytes)?,
        })
    }

    /// From the client's "blinded"" password, returns a challenge to be
    /// sent back to the client, as well as a ServerLogin
    ///
    /// # Arguments
    /// * `message`   - the initial registration message
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::{ClientLogin, ClientLoginStartParameters, ServerLogin, ServerLoginStartParameters};
    /// # use opaque_ke::{ClientRegistration, ClientRegistrationFinishParameters, ServerRegistration};
    /// # use opaque_ke::errors::ProtocolError;
    /// # use opaque_ke::keypair::KeyPair;
    /// use rand::{rngs::OsRng, RngCore};
    /// use opaque_ke::ciphersuite::CipherSuite;
    /// struct Default;
    /// impl CipherSuite for Default {
    ///     type Group = curve25519_dalek::ristretto::RistrettoPoint;
    ///     type KeyExchange = opaque_ke::key_exchange::tripledh::TripleDH;
    ///     type Hash = sha2::Sha512;
    ///     type SlowHash = opaque_ke::slow_hash::NoOpHash;
    /// }
    /// let mut client_rng = OsRng;
    /// let mut server_rng = OsRng;
    /// let server_kp = Default::generate_random_keypair(&mut server_rng);
    /// # let client_registration_start_result = ClientRegistration::<Default>::start(&mut client_rng, b"hunter2")?;
    /// # let server_registration_start_result = ServerRegistration::<Default>::start(&mut server_rng, client_registration_start_result.message, server_kp.public())?;
    /// # let client_registration_finish_result = client_registration_start_result.state.finish(&mut client_rng, server_registration_start_result.message, ClientRegistrationFinishParameters::default())?;
    /// # let p_file = server_registration_start_result.state.finish(client_registration_finish_result.message)?;
    /// let client_login_start_result = ClientLogin::<Default>::start(&mut client_rng, b"hunter2", ClientLoginStartParameters::default())?;
    /// let server_login_start_result = ServerLogin::start(&mut server_rng, p_file, &server_kp.private(), client_login_start_result.message, ServerLoginStartParameters::default())?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn start<R: RngCore + CryptoRng>(
        rng: &mut R,
        password_file: ServerRegistration<CS>,
        server_s_sk: &PrivateKey,
        l1: CredentialRequest<CS>,
        params: ServerLoginStartParameters,
    ) -> Result<ServerLoginStartResult<CS>, ProtocolError> {
        let client_s_pk = password_file
            .client_s_pk
            .clone()
            .ok_or(InternalPakeError::SealError)?;

        let (e_info, optional_ids) = match params {
            ServerLoginStartParameters::WithInfo(e_info) => (e_info, None),
            ServerLoginStartParameters::WithIdentifiers(id_u, id_s) => {
                (Vec::new(), Some((id_u, id_s)))
            }
            ServerLoginStartParameters::WithInfoAndIdentifiers(e_info, id_u, id_s) => {
                (e_info, Some((id_u, id_s)))
            }
        };

        let envelope = password_file
            .envelope
            .clone()
            .ok_or(InternalPakeError::SealError)?;
        if envelope.get_mode() != mode_from_ids(&optional_ids) {
            return Err(InternalPakeError::IncompatibleEnvelopeModeError.into());
        }

        let (id_u, id_s) = match optional_ids {
            None => (
                client_s_pk.to_arr().to_vec(),
                KeyPair::<CS::Group>::public_from_private(server_s_sk)
                    .to_arr()
                    .to_vec(),
            ),
            Some((id_u, id_s)) => (id_u, id_s),
        };

        let l1_bytes = &l1.serialize();
        let beta = oprf::evaluate(l1.alpha, &password_file.oprf_key);
        let server_s_pk = KeyPair::<CS::Group>::public_from_private(server_s_sk);

        let credential_response_component =
            CredentialResponse::<CS>::serialize_without_ke(&beta, &server_s_pk, &envelope);

        let (plain_info, ke2_state, ke2_message) = CS::KeyExchange::generate_ke2(
            rng,
            l1_bytes.to_vec(),
            credential_response_component,
            l1.ke1_message,
            client_s_pk,
            server_s_sk.clone(),
            id_u,
            id_s,
            e_info,
        )?;

        let credential_response = CredentialResponse {
            beta,
            server_s_pk,
            envelope,
            ke2_message,
        };

        Ok(ServerLoginStartResult {
            plain_info,
            message: credential_response,
            state: Self {
                _cs: PhantomData,
                ke2_state,
            },
        })
    }

    /// From the client's second and final message, check the client's
    /// authentication and produce a message transport
    ///
    /// # Arguments
    /// * `message` - the client's second login message
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::{ClientLogin, ClientLoginFinishParameters, ClientLoginStartParameters, ServerLogin, ServerLoginStartParameters};
    /// # use opaque_ke::{ClientRegistration, ClientRegistrationFinishParameters, ServerRegistration};
    /// # use opaque_ke::errors::ProtocolError;
    /// # use opaque_ke::keypair::KeyPair;
    /// use rand::{rngs::OsRng, RngCore};
    /// use opaque_ke::ciphersuite::CipherSuite;
    /// struct Default;
    /// impl CipherSuite for Default {
    ///     type Group = curve25519_dalek::ristretto::RistrettoPoint;
    ///     type KeyExchange = opaque_ke::key_exchange::tripledh::TripleDH;
    ///     type Hash = sha2::Sha512;
    ///     type SlowHash = opaque_ke::slow_hash::NoOpHash;
    /// }
    /// let mut client_rng = OsRng;
    /// let mut server_rng = OsRng;
    /// let server_kp = Default::generate_random_keypair(&mut server_rng);
    /// # let client_registration_start_result = ClientRegistration::<Default>::start(&mut client_rng, b"hunter2")?;
    /// # let server_registration_start_result = ServerRegistration::<Default>::start(&mut server_rng, client_registration_start_result.message, server_kp.public())?;
    /// # let client_registration_finish_result = client_registration_start_result.state.finish(&mut client_rng, server_registration_start_result.message, ClientRegistrationFinishParameters::default())?;
    /// # let p_file = server_registration_start_result.state.finish(client_registration_finish_result.message)?;
    /// let client_login_start_result = ClientLogin::<Default>::start(&mut client_rng, b"hunter2", ClientLoginStartParameters::default())?;
    /// let server_login_start_result = ServerLogin::start(&mut server_rng, p_file, &server_kp.private(), client_login_start_result.message, ServerLoginStartParameters::default())?;
    /// let client_login_finish_result = client_login_start_result.state.finish(server_login_start_result.message, ClientLoginFinishParameters::default())?;
    /// let mut server_transport = server_login_start_result.state.finish(client_login_finish_result.message)?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn finish(
        self,
        message: CredentialFinalization<CS>,
    ) -> Result<ServerLoginFinishResult<CS>, ProtocolError> {
        let session_key = <CS::KeyExchange as KeyExchange<CS::Hash, CS::Group>>::finish_ke(
            message.ke3_message,
            &self.ke2_state,
        )
        .map_err(|e| match e {
            ProtocolError::VerificationError(PakeError::KeyExchangeMacValidationError) => {
                ProtocolError::VerificationError(PakeError::InvalidLoginError)
            }
            err => err,
        })?;

        Ok(ServerLoginFinishResult {
            session_key,
            _cs: PhantomData,
            #[cfg(test)]
            state: self,
        })
    }

    #[cfg(test)]
    pub fn as_byte_ptrs(&self) -> Vec<(*const u8, usize)> {
        self.ke2_state.as_byte_ptrs()
    }
}

impl_serialize_and_deserialize_for!(ServerLogin);

// Zeroize on drop implementations

// This can't be derived because of the use of a phantom parameter
impl<CS: CipherSuite> Zeroize for ClientRegistration<CS> {
    fn zeroize(&mut self) {
        self.token.data.zeroize();
        self.token.blind.zeroize();
    }
}

impl<CS: CipherSuite> Drop for ClientRegistration<CS> {
    fn drop(&mut self) {
        self.zeroize();
    }
}

// This can't be derived because of the use of a phantom parameter
impl<CS: CipherSuite> Zeroize for ServerRegistration<CS> {
    fn zeroize(&mut self) {
        self.envelope.zeroize();
        self.client_s_pk.zeroize();
        self.oprf_key.zeroize();
    }
}

impl<CS: CipherSuite> Drop for ServerRegistration<CS> {
    fn drop(&mut self) {
        self.zeroize();
    }
}

// This can't be derived because of the use of a phantom parameter
impl<CS: CipherSuite> Zeroize for ClientLogin<CS> {
    fn zeroize(&mut self) {
        self.token.data.zeroize();
        self.token.blind.zeroize();
        self.ke1_state.zeroize();
        self.serialized_credential_request.zeroize();
    }
}

impl<CS: CipherSuite> Drop for ClientLogin<CS> {
    fn drop(&mut self) {
        self.zeroize();
    }
}

// This can't be derived because of the use of a phantom parameter
impl<CS: CipherSuite> Zeroize for ServerLogin<CS> {
    fn zeroize(&mut self) {
        self.ke2_state.zeroize();
    }
}

impl<CS: CipherSuite> Drop for ServerLogin<CS> {
    fn drop(&mut self) {
        self.zeroize();
    }
}

// Helper functions

fn get_password_derived_key<G: GroupWithMapToCurve, SH: SlowHash<D>, D: Hash>(
    token: &oprf::Token<G>,
    beta: G,
) -> Result<Vec<u8>, InternalPakeError> {
    let oprf_output = oprf::finalize::<G, D>(&token.data, &token.blind, beta);
    SH::hash(oprf_output)
}
