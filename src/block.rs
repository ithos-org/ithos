use adapter::Adapter;
use algorithm::{CipherSuite, DigestAlgorithm};
use buffoon::{OutputStream, Serialize};
use error::{Error, Result};
use object::Object;
use object::credential::CredentialEntry;
use object::domain::DomainEntry;
use object::ou::OrgUnitEntry;
use object::root::RootEntry;
use object::system::SystemEntry;
use objecthash::{self, ObjectHash, ObjectHasher};
use op::{self, Op};
use path::PathBuf;
use proto::ToProto;
use rustc_serialize::base64::{self, ToBase64};
use signature::KeyPair;
use std::io;
use timestamp::Timestamp;
use witness::Witness;

const DIGEST_SIZE: usize = 32;
const ADMIN_KEYPAIR_LIFETIME: u64 = 315_532_800; // 10 years

// Block IDs are presently SHA-256 only
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub struct Id([u8; DIGEST_SIZE]);

impl Id {
    // Parent ID of the initial block (256-bits of zero)
    pub fn zero() -> Id {
        Id([0u8; DIGEST_SIZE])
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Id> {
        if bytes.len() != DIGEST_SIZE {
            return Err(Error::parse(None));
        }

        let mut id = [0u8; DIGEST_SIZE];
        id.copy_from_slice(&bytes[0..DIGEST_SIZE]);

        Ok(Id(id))
    }
}

impl AsRef<[u8]> for Id {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl ObjectHash for Id {
    fn objecthash<H: ObjectHasher>(&self, hasher: &mut H) {
        self.0.objecthash(hasher);
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct Body {
    pub parent_id: Id,
    pub timestamp: Timestamp,
    pub ops: Vec<Op>,
    pub comment: String,
}

impl Body {
    // Create the first block in a new log, with a parent ID of zero.
    // This block contains the initial administrative signature key which will
    // be used as the initial root authority for new blocks in the log.
    pub fn create_initial(digest_alg: DigestAlgorithm,
                          admin_username: &str,
                          admin_signing_credential: CredentialEntry,
                          timestamp: Timestamp,
                          comment: &str)
                          -> Body {
        // SHA256 is the only algorithm we presently support
        assert!(digest_alg == DigestAlgorithm::Sha256);

        let mut ops = Vec::new();
        let mut path = PathBuf::new();

        ops.push(Op::new(op::Type::Add,
                         path.clone(),
                         Object::Root(RootEntry::new(digest_alg))));

        let global_domain = DomainEntry::new(Some(String::from("Global system users and config")));

        path.push("global");
        ops.push(Op::new(op::Type::Add, path.clone(), Object::Domain(global_domain)));

        let global_users_ou = OrgUnitEntry::new(Some(String::from("Core system users")));

        path.push("users");
        ops.push(Op::new(op::Type::Add,
                         path.clone(),
                         Object::OrgUnit(global_users_ou)));

        let admin_user = SystemEntry::new(String::from(admin_username));

        path.push(&admin_username);
        ops.push(Op::new(op::Type::Add, path.clone(), Object::System(admin_user)));

        let admin_keys_ou = OrgUnitEntry::new(Some(String::from("Admin credentials")));

        path.push("keys");
        ops.push(Op::new(op::Type::Add, path.clone(), Object::OrgUnit(admin_keys_ou)));

        // TODO: Verify we have a valid Ed25519 signing credential
        path.push("signing");
        ops.push(Op::new(op::Type::Add,
                         path,
                         Object::Credential(admin_signing_credential)));

        Body {
            parent_id: Id::zero(),
            timestamp: timestamp,
            ops: ops,
            comment: comment.to_string(),
        }
    }
}

impl ToProto for Body {}

impl Serialize for Body {
    fn serialize<O: OutputStream>(&self, out: &mut O) -> io::Result<()> {
        try!(out.write(1, self.parent_id.as_ref()));
        try!(out.write(2, &self.timestamp));
        try!(out.write_repeated(3, &self.ops));
        try!(out.write(4, &self.comment));
        Ok(())
    }
}

impl ObjectHash for Body {
    #[inline]
    fn objecthash<H: ObjectHasher>(&self, hasher: &mut H) {
        objecthash_struct!(
            hasher,
            "parent" => self.parent_id,
            "timestamp" => self.timestamp,
            "ops" => self.ops,
            "comment" => self.comment
        )
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct Block {
    pub body: Body,
    pub witness: Witness,
}

impl Block {
    // Create the first block in a new log, with a parent ID of zero.
    // The block is self-signed with the initial administrator key.
    pub fn create_initial(ciphersuite: CipherSuite,
                          admin_username: &str,
                          admin_keypair: &KeyPair,
                          admin_keypair_sealed: &[u8],
                          admin_keypair_salt: &[u8],
                          comment: &str)
                          -> Block {
        let timestamp = Timestamp::now();

        let admin_signing_credential =
            CredentialEntry::from_signature_keypair(ciphersuite.signature_alg(),
                                                    ciphersuite.encryption_alg(),
                                                    admin_keypair_sealed,
                                                    admin_keypair_salt,
                                                    admin_keypair.public_key_bytes(),
                                                    timestamp,
                                                    timestamp.extend(ADMIN_KEYPAIR_LIFETIME),
                                                    Some(String::from("Root signing key")));

        let body = Body::create_initial(ciphersuite.digest_alg(),
                                        admin_username,
                                        admin_signing_credential,
                                        timestamp,
                                        comment);

        Block::new(body, admin_keypair)
    }

    pub fn new(body: Body, keypair: &KeyPair) -> Block {
        let mut message = String::from("ithos.block.body.ni:///sha-256;");
        message.push_str(&objecthash::digest(&body).as_ref().to_base64(base64::URL_SAFE));

        Block {
            body: body,
            witness: Witness { signatures: vec![keypair.sign(&message.as_bytes())] },
        }
    }

    // Compute the Id of this block
    pub fn id(&self) -> Id {
        Id::from_bytes(objecthash::digest(self).as_ref()).unwrap()
    }

    // Parent ID of this block
    pub fn parent_id(&self) -> Id {
        self.body.parent_id
    }

    // Apply the operations contained within the block to the database
    pub fn apply<'a, A>(&self, adapter: &A, txn: &mut A::W) -> Result<()>
        where A: Adapter<'a>
    {
        let block_id = self.id();
        let mut state = op::State::new(try!(adapter.next_free_entry_id(txn)));

        // NOTE: This only stores the block in the database. It does not process it
        try!(adapter.add_block(txn, self));

        let ops = &self.body.ops;

        // Process the operations in the block and apply them to the database
        for op in ops {
            try!(op.apply(adapter, txn, &mut state, &block_id, self.body.timestamp));
        }

        Ok(())
    }
}

impl ToProto for Block {}

impl Serialize for Block {
    fn serialize<O: OutputStream>(&self, out: &mut O) -> io::Result<()> {
        try!(out.write(1, &self.body));
        try!(out.write(2, &self.witness));
        Ok(())
    }
}

impl ObjectHash for Block {
    #[inline]
    fn objecthash<H: ObjectHasher>(&self, hasher: &mut H) {
        objecthash_struct!(
            hasher,
            "body" => self.body,
            "witness" => self.witness
        )
    }
}

#[cfg(test)]
pub mod tests {
    use algorithm::CipherSuite;
    use block::Block;
    use buffoon;
    use ring::rand;
    use signature::KeyPair;

    const ADMIN_USERNAME: &'static str = "manager";
    const ADMIN_KEYPAIR_SEALED: &'static [u8] = b"placeholder";
    const ADMIN_KEYPAIR_SALT: &'static [u8] = b"NaCl";
    const COMMENT: &'static str = "The tree of a thousand users begins with a single block";

    pub fn example_block() -> Block {
        let rng = rand::SystemRandom::new();
        let admin_keypair = KeyPair::generate(&rng);

        Block::create_initial(CipherSuite::Ed25519Aes256GcmSha256,
                              ADMIN_USERNAME,
                              &admin_keypair,
                              ADMIN_KEYPAIR_SEALED,
                              ADMIN_KEYPAIR_SALT,
                              COMMENT)
    }

    #[test]
    fn test_proto_serialization() {
        let block = example_block();
        // TODO: better test of the serialized proto
        buffoon::serialize(&block).unwrap();
    }
}
