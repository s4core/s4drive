//! In-memory object store for unit tests.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::error::{CoreError, CoreResult};
use crate::s3::{ObjectMeta, S3ObjectStore};

#[derive(Default)]
pub struct MemoryStore {
    objects: Mutex<BTreeMap<String, (Vec<u8>, String)>>,
}

impl MemoryStore {
    pub fn keys(&self, prefix: &str) -> Vec<String> {
        self.objects
            .lock()
            .unwrap()
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect()
    }

    fn etag(body: &[u8]) -> String {
        blake3::hash(body).to_hex()[..32].to_string()
    }
}

impl S3ObjectStore for MemoryStore {
    fn bucket(&self) -> &str {
        "memory"
    }

    fn endpoint(&self) -> &str {
        "memory://"
    }

    async fn put_object(&self, key: &str, body: Vec<u8>) -> CoreResult<String> {
        let etag = Self::etag(&body);
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), (body, etag.clone()));
        Ok(etag)
    }

    async fn put_if_not_exists(&self, key: &str, body: Vec<u8>) -> CoreResult<bool> {
        let mut objects = self.objects.lock().unwrap();
        if objects.contains_key(key) {
            return Ok(false);
        }
        let etag = Self::etag(&body);
        objects.insert(key.to_string(), (body, etag));
        Ok(true)
    }

    async fn put_if_match(
        &self,
        key: &str,
        body: Vec<u8>,
        expected_etag: &str,
    ) -> CoreResult<bool> {
        let mut objects = self.objects.lock().unwrap();
        match objects.get_mut(key) {
            Some((stored, etag)) if etag == expected_etag.trim_matches('"') => {
                *etag = Self::etag(&body);
                *stored = body;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn get_object(&self, key: &str) -> CoreResult<Vec<u8>> {
        self.objects
            .lock()
            .unwrap()
            .get(key)
            .map(|(body, _)| body.clone())
            .ok_or_else(|| CoreError::NotFound(key.to_string()))
    }

    async fn get_object_range(&self, key: &str, _range: &str) -> CoreResult<Vec<u8>> {
        self.get_object(key).await
    }

    async fn head_object(&self, key: &str) -> CoreResult<ObjectMeta> {
        self.objects
            .lock()
            .unwrap()
            .get(key)
            .map(|(body, etag)| ObjectMeta {
                key: key.to_string(),
                size: body.len() as u64,
                etag: etag.clone(),
                last_modified: String::new(),
                version_id: None,
            })
            .ok_or_else(|| CoreError::NotFound(key.to_string()))
    }

    async fn delete_object(&self, key: &str) -> CoreResult<()> {
        self.objects.lock().unwrap().remove(key);
        Ok(())
    }

    async fn list_objects(&self, prefix: &str) -> CoreResult<Vec<String>> {
        Ok(self.keys(prefix))
    }
}
