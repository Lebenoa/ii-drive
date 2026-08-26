mod botfather;
mod files;
mod internal_db;
mod locales;
mod session;

pub use files::{
    create_folder, delete_file, delete_folder, file_link, file_thumb, health, limits, list_files,
    list_folders, media_token, move_file, raw_file, set_visibility, share_meta, upload_abort,
    upload_bench, upload_chunk, upload_complete, upload_file, upload_init, upload_limit,
    upload_status,
};

pub use botfather::{
    bots as botfather_bots, cancel_draft as botfather_cancel, draft as botfather_draft,
    send as botfather_send, token as botfather_token,
};
pub use session::{
    add_bot, auth_code, auth_logout, auth_password, auth_phone, avatar, create_channel,
    get_instance, get_rules, get_settings, list_bots, list_channels, me, remove_bot, save_instance,
    save_rules, save_settings, select_channels, sweep_thumbs,
};

/// The thumbnail sweeper is also driven by the startup/interval loop in
/// `main`, not just the operator endpoint.
pub use files::{next_sweep_in, sweep};
pub use internal_db::{query as internal_db_query, tables as internal_db_tables};
pub use locales::{locale as locale_file, manifest as locale_manifest};
