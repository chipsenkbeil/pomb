use redb::{ReadableTable, WriteTransaction};

use super::usage::{txn_count_usage, txn_refuse_over_bytes};
use super::{
    ACCOUNT_SETTINGS, AVATARS, Db, DbResult, txn_read_one, txn_refuse_stranded, write_one,
};
use crate::models::{AccountId, Avatar, AvatarKind, Settings};

impl Db {
    /// Returns the bytes of `account`'s uploaded avatar image, absent without one.
    pub fn avatar_image(&self, account: &AccountId) -> DbResult<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(AVATARS)?;
        let image = table
            .get(account.as_str())?
            .map(|guard| guard.value().to_vec());
        Ok(image)
    }

    /// Stores `image` as `account`'s avatar and makes the image what the avatar shows. The bytes
    /// count against the account's stored-bytes quota, and an image the quota has no room for is
    /// refused.
    pub fn set_avatar_image(&self, account: &AccountId, image: &[u8]) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        txn_refuse_stranded(&txn, account)?;
        let held = txn
            .open_table(AVATARS)?
            .get(account.as_str())?
            .map_or(0, |guard| guard.value().len() as u64);
        let added = image.len() as u64;
        txn_refuse_over_bytes(&txn, account.as_str(), added, held)?;
        txn_count_usage(&txn, account.as_str(), added, held)?;
        txn.open_table(AVATARS)?.insert(account.as_str(), image)?;
        let mut settings =
            txn_read_one::<Settings>(&txn, ACCOUNT_SETTINGS, account.as_str())?.unwrap_or_default();
        settings.avatar.kind = AvatarKind::Image;
        settings.avatar.image_version = settings.avatar.image_version.saturating_add(1);
        write_one(&txn, ACCOUNT_SETTINGS, account.as_str(), &settings)?;
        txn.commit()?;
        Ok(())
    }

    /// Stores `settings` with `avatar` as the account's text avatar in one write, and deletes the
    /// uploaded image, taking its bytes off the account. The avatar keeps the stored image
    /// version, so a later upload never reuses a version a browser cached.
    pub fn update_settings_with_text_avatar(
        &self,
        account: &AccountId,
        settings: &Settings,
        avatar: Avatar,
    ) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        txn_refuse_stranded(&txn, account)?;
        let stored = txn_read_one::<Settings>(&txn, ACCOUNT_SETTINGS, account.as_str())?;
        let mut settings = settings.clone();
        settings.avatar = Avatar {
            image_version: stored
                .as_ref()
                .map_or(0, |stored| stored.avatar.image_version),
            ..avatar
        };
        settings.check_caps(stored.as_ref())?;
        write_one(&txn, ACCOUNT_SETTINGS, account.as_str(), &settings)?;
        txn_drop_avatar_image(&txn, account)?;
        txn.commit()?;
        Ok(())
    }
}

/// Removes the uploaded avatar image of `account` inside an open write transaction, and takes its
/// bytes off the account.
fn txn_drop_avatar_image(txn: &WriteTransaction, account: &AccountId) -> DbResult<()> {
    let removed = txn
        .open_table(AVATARS)?
        .remove(account.as_str())?
        .map_or(0, |guard| guard.value().len() as u64);
    txn_count_usage(txn, account.as_str(), 0, removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::BreakActivity;

    #[test]
    fn a_text_avatar_should_save_over_a_stored_setting_past_a_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("avatar.redb").to_str().expect("utf8")).expect("db");
        let account = db.create_user("admin").expect("admin");
        let stored = Settings {
            break_activities: vec![BreakActivity {
                id: "act-old".to_string(),
                name: "Walk".to_string(),
                description: "d".repeat(300),
                daily_goal: 1,
                daily_limit: None,
            }],
            ..Settings::default()
        };
        let txn = db.db.begin_write().expect("write");
        write_one(&txn, ACCOUNT_SETTINGS, account.as_str(), &stored).expect("settings");
        txn.commit().expect("commit");
        db.set_avatar_image(&account, b"image").expect("image");

        let mut edited = db.settings(&account).expect("settings");
        edited.dismissed_hints.push("members".to_string());
        let text = Avatar {
            text: "AB".to_string(),
            ..Avatar::default()
        };
        db.update_settings_with_text_avatar(&account, &edited, text)
            .expect("the stored text past the cap stays savable");

        let kept = db.settings(&account).expect("settings");
        assert_eq!(kept.avatar.kind, AvatarKind::Text);
        assert_eq!(kept.avatar.text, "AB");
        assert_eq!(kept.avatar.image_version, 1);
        assert_eq!(kept.dismissed_hints, vec!["members".to_string()]);
        assert!(db.avatar_image(&account).expect("read").is_none());
    }
}
