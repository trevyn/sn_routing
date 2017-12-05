// Copyright 2015 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net
// Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3,
// depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project
// generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.0 This,
// along with the
// Licenses can be found in the root directory of this project at LICENSE,
// COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network
// Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES
// OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions
// and limitations
// relating to use of the SAFE Network Software.

use error::RoutingError;
use peer_id::PeerId;
use proof::Proof;
use rust_sodium::crypto::sign::PublicKey;
use sha3::Digest256;
use std::collections::HashSet;
use vote::Vote;


/// A `Block` *is* network consensus. It covers a group of nodes closest to an
/// address and is signed
/// With quorum valid votes, the consensus is then valid and therefor the
/// `Block` however itsworth
/// recodnising quorum is the weakest consensus as any differnce on network
/// view will break it.
/// Full group consensus is strongest, but likely unachievable most of the
/// time, so a union
/// can increase a single `Peer`s quorum valid `Block`
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Block {
    payload: Digest256,
    proofs: HashSet<Proof>,
}

impl Block {
    /// A new `Block` requires a valid vote and the `PublicKey` of the node who
    /// sent us this.
    /// For this reason The `Vote` will require a Direct Message from a `Peer`
    /// to us.
    #[allow(unused)]
    pub fn new(vote: &Vote, pub_key: &PublicKey, age: u8) -> Result<Block, RoutingError> {
        let peer_id = PeerId::new(age, pub_key);
        if !vote.validate_signature(&peer_id) {
            return Err(RoutingError::FailedSignature);
        }
        let proof = Proof::new(&pub_key, age, vote)?;
        let mut proofset = HashSet::<Proof>::new();
        if !proofset.insert(proof) {
            return Err(RoutingError::FailedSignature);
        }
        Ok(Block {
            payload: vote.payload().clone(),
            proofs: proofset,
        })
    }

    /// Add a proof from a peer when we know we have an existing `Block`
    #[allow(unused)]
    pub fn add_proof(&mut self, proof: Proof) -> Result<(), RoutingError> {
        if !proof.validate_signature(&self.payload) {
            return Err(RoutingError::FailedSignature);
        }
        if self.proofs.insert(proof) {
            return Ok(());
        }
        Err(RoutingError::FailedSignature)
    }

    /// We may wish to remove a nodes `Proof` in cases where a `Peer` cannot be
    /// considered valid
    #[allow(unused)]
    pub fn remove_proof(&mut self, pub_key: &PublicKey) {
        self.proofs.retain(|proof| proof.key() != pub_key)
    }

    /// Ensure only the following `Peer`s are considered in the `Block`,
    /// Prune any that are not in this set.
    #[allow(unused)]
    pub fn prune_proofs_except(&mut self, mut keys: &HashSet<&PublicKey>) {
        self.proofs.retain(|proof| keys.contains(proof.key()));
    }

    /// Return numbes of `Proof`s
    #[allow(unused)]
    pub fn total_proofs(&self) -> usize {
        self.proofs.iter().count()
    }

    /// Return numbes of `Proof`s
    #[allow(unused)]
    pub fn total_proofs_age(&self) -> usize {
        self.proofs.iter().fold(0, |total, ref proof| {
            total + proof.age() as usize
        })
    }

    #[allow(unused)]
    /// getter
    pub fn proofs(&self) -> &HashSet<Proof> {
        &self.proofs
    }

    #[allow(unused)]
    /// getter
    pub fn payload(&self) -> &Digest256 {
        &self.payload
    }
}

#[cfg(test)]

mod tests {
    use super::*;
    use maidsafe_utilities::SeededRng;
    use rand::Rng;
    use rust_sodium;
    use rust_sodium::crypto::sign;
    use tiny_keccak::sha3_256;



    #[test]
    fn create_then_remove_add_proofs() {
        let mut rng = SeededRng::thread_rng();
        unwrap!(rust_sodium::init_with_rng(&mut rng));

        let keys0 = sign::gen_keypair();
        let keys1 = sign::gen_keypair();
        let peer_id0 = PeerId::new(rng.gen_range(0, 255), keys0.0);
        let peer_id1 = PeerId::new(rng.gen_range(0, 255), keys1.0);
        let payload = sha3_256(b"1");
        let vote0 = unwrap!(Vote::new(&keys0.1, payload));
        assert!(vote0.validate_signature(&peer_id0));
        let vote1 = unwrap!(Vote::new(&keys1.1, payload));
        assert!(vote1.validate_signature(&peer_id1));
        let proof0 = unwrap!(Proof::new(&keys0.0, rng.gen_range(0, 255), &vote0));
        assert!(proof0.validate_signature(&payload));
        let proof1 = unwrap!(Proof::new(&keys1.0, rng.gen_range(0, 255), &vote1));
        assert!(proof1.validate_signature(&payload));
        let mut b0 = unwrap!(Block::new(&vote0, &keys0.0, rng.gen_range(0, 255)));
        assert!(proof0.validate_signature(&b0.payload));
        assert!(proof1.validate_signature(&b0.payload));
        assert!(b0.total_proofs() == 1);
        b0.remove_proof(&keys0.0);
        assert!(b0.total_proofs() == 0);
        assert!(b0.add_proof(proof0).is_ok());
        assert!(b0.total_proofs() == 1);
        assert!(b0.add_proof(proof1).is_ok());
        assert!(b0.total_proofs() == 2);
        b0.remove_proof(&keys1.0);
        assert!(b0.total_proofs() == 1);
    }

    #[test]
    fn confirm_new_proof_batch() {
        let mut rng = SeededRng::thread_rng();
        unwrap!(rust_sodium::init_with_rng(&mut rng));

        let keys0 = sign::gen_keypair();
        let keys1 = sign::gen_keypair();
        let keys2 = sign::gen_keypair();
        let payload = sha3_256(b"1");
        let vote0 = unwrap!(Vote::new(&keys0.1, payload));
        let vote1 = unwrap!(Vote::new(&keys1.1, payload));
        let vote2 = unwrap!(Vote::new(&keys2.1, payload));
        let proof1 = unwrap!(Proof::new(&keys1.0, rng.gen_range(0, 255), &vote1));
        let proof2 = unwrap!(Proof::new(&keys2.0, rng.gen_range(0, 255), &vote2));
        // So 3 votes all valid will be added to block
        let mut b0 = unwrap!(Block::new(&vote0, &keys0.0, rng.gen_range(0, 255)));
        assert!(b0.add_proof(proof1).is_ok());
        assert!(b0.add_proof(proof2).is_ok());
        assert!(b0.total_proofs() == 3);
        // All added validly, so now only use 2 of these
        let mut my_known_nodes = HashSet::<&PublicKey>::new();
        assert!(my_known_nodes.insert(&keys0.0));
        assert!(my_known_nodes.insert(&keys1.0));
        b0.prune_proofs_except(&my_known_nodes);
        assert!(b0.total_proofs() == 2);

    }

}
