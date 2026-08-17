fn main() {
    println!("cargo:rerun-if-env-changed=BNETSWITCHLITE_BLIZZARD_TEAM_ID");
    let target = std::env::var("TARGET").unwrap_or_default();
    let profile = std::env::var("PROFILE").unwrap_or_default();
    if target.ends_with("apple-darwin") {
        match std::env::var("BNETSWITCHLITE_BLIZZARD_TEAM_ID") {
            Ok(team_id)
                if team_id.len() == 10
                    && team_id.bytes().all(|byte| byte.is_ascii_alphanumeric()) =>
            {
                println!("cargo:rustc-env=BNETSWITCHLITE_BLIZZARD_TEAM_ID={team_id}");
            }
            _ if profile == "release" => {
                panic!(
                    "macOS release builds require a verified 10-character BNETSWITCHLITE_BLIZZARD_TEAM_ID"
                );
            }
            _ => {}
        }
    }
    tauri_build::build()
}
