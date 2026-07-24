//! Image user, group, home-directory, and subordinate-ID normalization.

use super::*;

/// Normalizes the OCI identity and its conventional supplementary groups.
pub(crate) fn normalize_image_user(
    root: &Path,
    config: ImageConfig,
) -> Result<(ImageConfig, bool), ImageBuildError> {
    normalize_image_user_with(root, config, set_directory_owner)
}

pub(crate) fn normalize_image_user_with<F>(
    root: &Path,
    config: ImageConfig,
    set_owner: F,
) -> Result<(ImageConfig, bool), ImageBuildError>
where
    F: FnOnce(&Path, u32, u32) -> Result<(), ImageBuildError>,
{
    let (config, identity_normalized) = if config.user().uid() == 0 {
        normalize_root_image_user(root, &config, set_owner)
    } else {
        normalize_non_root_image_user(root, config)
    }?;
    let subordinate_ids_normalized = normalize_subordinate_id_files(root, config.user())?;
    Ok((config, identity_normalized || subordinate_ids_normalized))
}

pub(crate) fn normalize_root_image_user<F>(
    root: &Path,
    config: &ImageConfig,
    set_owner: F,
) -> Result<(ImageConfig, bool), ImageBuildError>
where
    F: FnOnce(&Path, u32, u32) -> Result<(), ImageBuildError>,
{
    let etc = ensure_real_directory_tree(root, Path::new("etc"))?;
    let passwd_path = etc.join("passwd");
    let group_path = etc.join("group");
    let passwd_contents = read_account_database(&passwd_path)?;
    let group_contents = read_account_database(&group_path)?;
    let accounts = parse_passwd_accounts(passwd_contents.as_deref(), &passwd_path)?;
    let groups = parse_group_accounts(group_contents.as_deref(), &group_path)?;

    let account = if let Some(account) = select_development_account(&accounts)? {
        account
    } else {
        let gid = development_group_id(&groups)?;
        if !groups.iter().any(|group| group.gid == gid) {
            append_account_record(
                &group_path,
                group_contents.as_deref(),
                &format!("{DEVELOPMENT_USER_NAME}:x:{gid}:"),
            )?;
        }
        let account = PasswdAccount {
            name: DEVELOPMENT_USER_NAME.to_owned(),
            uid: DEVELOPMENT_USER_ID,
            gid,
            home: DEVELOPMENT_USER_HOME.to_owned(),
        };
        append_account_record(
            &passwd_path,
            passwd_contents.as_deref(),
            &format!(
                "{}:x:{}:{}:Tascarrel development user:{}:/bin/sh",
                account.name, account.uid, account.gid, account.home
            ),
        )?;
        account
    };
    if account.uid == 0 {
        return Err(ImageBuildError::InvalidImageConfig(
            "the selected development account resolves to root".to_owned(),
        ));
    }
    validate_home(&account.home)?;
    let home = ensure_real_directory_tree(
        root,
        Path::new(&account.home)
            .strip_prefix("/")
            .expect("validated image user home is absolute"),
    )?;
    set_owner(&home, account.uid, account.gid)?;

    let mut additional_gids = groups
        .iter()
        .filter(|group| {
            group.gid != account.gid && group.members.iter().any(|member| member == &account.name)
        })
        .map(|group| group.gid)
        .collect::<Vec<_>>();
    ensure_docker_group(
        &group_path,
        group_contents.as_deref(),
        &groups,
        Some(&account.name),
        account.gid,
        &mut additional_gids,
    )?;
    let user = ImageUser::new(
        account.name.clone(),
        account.uid,
        account.gid,
        additional_gids,
    )
    .map_err(|error| ImageBuildError::InvalidImageConfig(error.to_string()))?;
    let mut environment = config.environment().to_vec();
    environment.retain(|entry| {
        entry
            .split_once('=')
            .is_none_or(|(name, _)| !matches!(name, "HOME" | "USER" | "LOGNAME"))
    });
    environment.extend([
        format!("HOME={}", account.home),
        format!("USER={}", account.name),
        format!("LOGNAME={}", account.name),
    ]);
    let normalized = ImageConfig::for_process(environment, user, config.working_directory())
        .map_err(|error| ImageBuildError::InvalidImageConfig(error.to_string()))?;
    Ok((normalized, true))
}

pub(crate) fn normalize_non_root_image_user(
    root: &Path,
    config: ImageConfig,
) -> Result<(ImageConfig, bool), ImageBuildError> {
    let Some(etc) = existing_real_directory(&root.join("etc"), "image account directory")? else {
        return Ok((config, false));
    };
    let group_path = etc.join("group");
    let Some(group_contents) = read_account_database(&group_path)? else {
        return Ok((config, false));
    };
    let groups = parse_group_accounts(Some(&group_contents), &group_path)?;
    if docker_group(&groups)?.is_none() {
        return Ok((config, false));
    }
    let passwd_path = etc.join("passwd");
    let passwd_contents = read_account_database(&passwd_path)?;
    let accounts = parse_passwd_accounts(passwd_contents.as_deref(), &passwd_path)?;
    let account_name = account_name_for_image_user(&accounts, config.user())?;
    let mut additional_gids = config.user().additional_gids().to_vec();
    let group_changed = ensure_docker_group(
        &group_path,
        Some(&group_contents),
        &groups,
        account_name.as_deref(),
        config.user().gid(),
        &mut additional_gids,
    )?;
    let gids_changed = additional_gids.as_slice() != config.user().additional_gids();
    if !group_changed && !gids_changed {
        return Ok((config, false));
    }
    let user = ImageUser::new(
        config.user().name(),
        config.user().uid(),
        config.user().gid(),
        additional_gids,
    )
    .map_err(|error| ImageBuildError::InvalidImageConfig(error.to_string()))?;
    let normalized = ImageConfig::for_process(
        config.environment().iter().cloned(),
        user,
        config.working_directory(),
    )
    .map_err(|error| ImageBuildError::InvalidImageConfig(error.to_string()))?;
    Ok((normalized, true))
}

pub(crate) fn ensure_docker_group(
    path: &Path,
    contents: Option<&[u8]>,
    groups: &[GroupAccount],
    account_name: Option<&str>,
    primary_gid: u32,
    additional_gids: &mut Vec<u32>,
) -> Result<bool, ImageBuildError> {
    let Some(group) = docker_group(groups)? else {
        return Ok(false);
    };
    if group.gid == primary_gid {
        return Ok(false);
    }
    let mut changed = false;
    if !additional_gids.contains(&group.gid) {
        additional_gids.push(group.gid);
        changed = true;
    }
    if let Some(account_name) = account_name
        && !account_name.contains(',')
        && !group.members.iter().any(|member| member == account_name)
    {
        let contents = contents.ok_or_else(|| {
            unsafe_account_database(path, "Docker group has no backing group database")
        })?;
        add_group_member(path, contents, &group.name, account_name)?;
        changed = true;
    }
    Ok(changed)
}

/// Assigns the selected user the pod map's complete secondary ID block.
pub(crate) fn normalize_subordinate_id_files(
    root: &Path,
    user: &ImageUser,
) -> Result<bool, ImageBuildError> {
    let etc = ensure_real_directory_tree(root, Path::new("etc"))?;
    let record = format!("{}:{ID_MAP_SIZE}:{ID_MAP_SIZE}\n", user.name());
    let mut changed = false;
    for name in ["subuid", "subgid"] {
        let path = etc.join(name);
        let existing = read_account_database(&path)?;
        if existing.as_deref() != Some(record.as_bytes()) {
            write_account_database(&path, existing.as_deref(), record.as_bytes())?;
            changed = true;
        }
    }
    Ok(changed)
}

pub(crate) fn docker_group(
    groups: &[GroupAccount],
) -> Result<Option<&GroupAccount>, ImageBuildError> {
    let matches = groups
        .iter()
        .filter(|group| group.name == "docker")
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(ImageBuildError::InvalidImageConfig(
            "multiple image groups are named \"docker\"".to_owned(),
        ));
    }
    Ok(matches.first().copied())
}

pub(crate) fn account_name_for_image_user(
    accounts: &[PasswdAccount],
    user: &ImageUser,
) -> Result<Option<String>, ImageBuildError> {
    let named = accounts
        .iter()
        .filter(|account| account.name == user.name() && account.uid == user.uid())
        .collect::<Vec<_>>();
    if named.len() > 1 {
        return Err(ImageBuildError::InvalidImageConfig(format!(
            "multiple image accounts are named {:?}",
            user.name()
        )));
    }
    if let Some(account) = named.first() {
        return Ok(Some(account.name.clone()));
    }
    let by_id = accounts
        .iter()
        .filter(|account| account.uid == user.uid())
        .collect::<Vec<_>>();
    if by_id.len() == 1 {
        Ok(Some(by_id[0].name.clone()))
    } else {
        Ok(None)
    }
}

pub(crate) fn select_development_account(
    accounts: &[PasswdAccount],
) -> Result<Option<PasswdAccount>, ImageBuildError> {
    let by_id = accounts
        .iter()
        .filter(|account| account.uid == DEVELOPMENT_USER_ID)
        .collect::<Vec<_>>();
    if by_id.len() > 1 {
        return Err(ImageBuildError::InvalidImageConfig(format!(
            "multiple image accounts use development UID {DEVELOPMENT_USER_ID}"
        )));
    }
    if let Some(account) = by_id.first() {
        return Ok(Some((*account).clone()));
    }
    let by_name = accounts
        .iter()
        .filter(|account| account.name == DEVELOPMENT_USER_NAME)
        .collect::<Vec<_>>();
    if by_name.len() > 1 {
        return Err(ImageBuildError::InvalidImageConfig(format!(
            "multiple image accounts are named {DEVELOPMENT_USER_NAME:?}"
        )));
    }
    Ok(by_name.first().map(|account| (*account).clone()))
}

pub(crate) fn development_group_id(groups: &[GroupAccount]) -> Result<u32, ImageBuildError> {
    let named = groups
        .iter()
        .filter(|group| group.name == DEVELOPMENT_USER_NAME)
        .collect::<Vec<_>>();
    if named.len() > 1 {
        return Err(ImageBuildError::InvalidImageConfig(format!(
            "multiple image groups are named {DEVELOPMENT_USER_NAME:?}"
        )));
    }
    Ok(named.first().map_or(DEVELOPMENT_USER_ID, |group| group.gid))
}

pub(crate) fn parse_passwd_accounts(
    contents: Option<&[u8]>,
    path: &Path,
) -> Result<Vec<PasswdAccount>, ImageBuildError> {
    let Some(contents) = contents else {
        return Ok(Vec::new());
    };
    let text = std::str::from_utf8(contents)
        .map_err(|_| unsafe_account_database(path, "file is not valid UTF-8"))?;
    let mut accounts = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        if line.starts_with('#') {
            continue;
        }
        let fields = line.split(':').collect::<Vec<_>>();
        if fields.len() != 7 || fields[0].is_empty() {
            return Err(unsafe_account_database(
                path,
                "file contains a malformed record",
            ));
        }
        let uid = fields[2]
            .parse::<u32>()
            .map_err(|_| unsafe_account_database(path, "file contains an invalid UID"))?;
        let gid = fields[3]
            .parse::<u32>()
            .map_err(|_| unsafe_account_database(path, "file contains an invalid GID"))?;
        accounts.push(PasswdAccount {
            name: fields[0].to_owned(),
            uid,
            gid,
            home: fields[5].to_owned(),
        });
    }
    Ok(accounts)
}

pub(crate) fn parse_group_accounts(
    contents: Option<&[u8]>,
    path: &Path,
) -> Result<Vec<GroupAccount>, ImageBuildError> {
    let Some(contents) = contents else {
        return Ok(Vec::new());
    };
    let text = std::str::from_utf8(contents)
        .map_err(|_| unsafe_account_database(path, "file is not valid UTF-8"))?;
    let mut groups = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        if line.starts_with('#') {
            continue;
        }
        let fields = line.split(':').collect::<Vec<_>>();
        if fields.len() != 4 || fields[0].is_empty() {
            return Err(unsafe_account_database(
                path,
                "file contains a malformed record",
            ));
        }
        let gid = fields[2]
            .parse::<u32>()
            .map_err(|_| unsafe_account_database(path, "file contains an invalid GID"))?;
        groups.push(GroupAccount {
            name: fields[0].to_owned(),
            gid,
            members: fields[3]
                .split(',')
                .filter(|member| !member.is_empty())
                .map(str::to_owned)
                .collect(),
        });
    }
    Ok(groups)
}

pub(crate) fn read_account_database(path: &Path) -> Result<Option<Vec<u8>>, ImageBuildError> {
    let expected = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(unsafe_account_database(
                path,
                "path is not a real regular file",
            ));
        }
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ImageBuildError::Io {
                operation: "inspect image account database",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if expected.len() > MAX_ACCOUNT_DATABASE_BYTES {
        return Err(unsafe_account_database(path, "file is too large"));
    }
    let descriptor = open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|source| ImageBuildError::Io {
        operation: "open image account database",
        path: path.to_path_buf(),
        source: io::Error::from_raw_os_error(source.raw_os_error()),
    })?;
    let mut file = File::from(descriptor);
    let opened = file.metadata().map_err(|source| ImageBuildError::Io {
        operation: "inspect open image account database",
        path: path.to_path_buf(),
        source,
    })?;
    if !same_metadata(&expected, &opened) {
        return Err(unsafe_account_database(
            path,
            "file changed before it was opened",
        ));
    }
    let mut contents = Vec::with_capacity(usize::try_from(expected.len()).unwrap_or(0));
    Read::by_ref(&mut file)
        .take(MAX_ACCOUNT_DATABASE_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|source| ImageBuildError::Io {
            operation: "read image account database",
            path: path.to_path_buf(),
            source,
        })?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_ACCOUNT_DATABASE_BYTES {
        return Err(unsafe_account_database(
            path,
            "file grew beyond its size limit",
        ));
    }
    let after = file.metadata().map_err(|source| ImageBuildError::Io {
        operation: "reinspect image account database",
        path: path.to_path_buf(),
        source,
    })?;
    if !same_metadata(&opened, &after) {
        return Err(unsafe_account_database(
            path,
            "file changed while it was read",
        ));
    }
    Ok(Some(contents))
}

pub(crate) fn append_account_record(
    path: &Path,
    existing: Option<&[u8]>,
    record: &str,
) -> Result<(), ImageBuildError> {
    let mut contents = existing.unwrap_or_default().to_vec();
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        contents.push(b'\n');
    }
    contents.extend_from_slice(record.as_bytes());
    contents.push(b'\n');
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_ACCOUNT_DATABASE_BYTES {
        return Err(unsafe_account_database(path, "updated file is too large"));
    }
    write_account_database(path, existing, &contents)
}

pub(crate) fn add_group_member(
    path: &Path,
    existing: &[u8],
    group_name: &str,
    account_name: &str,
) -> Result<(), ImageBuildError> {
    let text = std::str::from_utf8(existing)
        .map_err(|_| unsafe_account_database(path, "file is not valid UTF-8"))?;
    let trailing_newline = text.ends_with('\n');
    let mut found = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            lines.push(line.to_owned());
            continue;
        }
        let mut fields = line.split(':').map(str::to_owned).collect::<Vec<_>>();
        if fields.len() != 4 || fields[0].is_empty() {
            return Err(unsafe_account_database(
                path,
                "file contains a malformed record",
            ));
        }
        if fields[0] == group_name {
            if found {
                return Err(ImageBuildError::InvalidImageConfig(format!(
                    "multiple image groups are named {group_name:?}"
                )));
            }
            found = true;
            if fields[3].is_empty() {
                fields[3].push_str(account_name);
            } else if !fields[3].split(',').any(|member| member == account_name) {
                fields[3].push(',');
                fields[3].push_str(account_name);
            }
        }
        lines.push(fields.join(":"));
    }
    if !found {
        return Err(unsafe_account_database(
            path,
            "Docker group disappeared before update",
        ));
    }
    let mut updated = lines.join("\n").into_bytes();
    if trailing_newline {
        updated.push(b'\n');
    }
    write_account_database(path, Some(existing), &updated)
}

pub(crate) fn write_account_database(
    path: &Path,
    existing: Option<&[u8]>,
    contents: &[u8],
) -> Result<(), ImageBuildError> {
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_ACCOUNT_DATABASE_BYTES {
        return Err(unsafe_account_database(path, "updated file is too large"));
    }

    let mut options = OpenOptions::new();
    options.write(true).mode(0o644);
    let mut file = if existing.is_some() {
        options
            .read(true)
            .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
            .open(path)
    } else {
        options.create_new(true).open(path)
    }
    .map_err(|source| ImageBuildError::Io {
        operation: "open image account database for update",
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| ImageBuildError::Io {
        operation: "inspect image account database for update",
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || existing.is_some_and(|previous| metadata.len() != previous.len() as u64)
    {
        return Err(unsafe_account_database(
            path,
            "file is linked or changed before update",
        ));
    }
    if existing.is_none() {
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .map_err(|source| ImageBuildError::Io {
                operation: "set image account database permissions",
                path: path.to_path_buf(),
                source,
            })?;
    }
    file.set_len(0)
        .and_then(|()| file.write_all(contents))
        .and_then(|()| file.sync_all())
        .map_err(|source| ImageBuildError::Io {
            operation: "update image account database",
            path: path.to_path_buf(),
            source,
        })
}

pub(crate) fn existing_real_directory(
    path: &Path,
    kind: &'static str,
) -> Result<Option<PathBuf>, ImageBuildError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(ImageBuildError::UnsafeOutput {
                kind,
                path: path.to_path_buf(),
                reason: "path is not a real directory",
            })
        }
        Ok(_) => Ok(Some(path.to_path_buf())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ImageBuildError::Io {
            operation: "inspect image account directory",
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub(crate) fn ensure_real_directory_tree(
    root: &Path,
    relative: &Path,
) -> Result<PathBuf, ImageBuildError> {
    real_directory(root, "normalized root filesystem")?;
    let mut directory = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(ImageBuildError::InvalidImageConfig(
                "image user directory is not normalized".to_owned(),
            ));
        };
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(ImageBuildError::UnsafeOutput {
                    kind: "image user directory",
                    path: directory,
                    reason: "path is not a real directory",
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                builder
                    .mode(0o755)
                    .create(&directory)
                    .map_err(|source| ImageBuildError::Io {
                        operation: "create image user directory",
                        path: directory.clone(),
                        source,
                    })?;
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).map_err(
                    |source| ImageBuildError::Io {
                        operation: "set image user directory permissions",
                        path: directory.clone(),
                        source,
                    },
                )?;
            }
            Err(source) => {
                return Err(ImageBuildError::Io {
                    operation: "inspect image user directory",
                    path: directory,
                    source,
                });
            }
        }
    }
    Ok(directory)
}

pub(crate) fn validate_home(home: &str) -> Result<(), ImageBuildError> {
    let home = Path::new(home);
    if home == Path::new("/")
        || !home.is_absolute()
        || home.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(ImageBuildError::InvalidImageConfig(
            "selected development account has an unsafe home directory".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn set_directory_owner(
    directory: &Path,
    uid: u32,
    gid: u32,
) -> Result<(), ImageBuildError> {
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
        .open(directory)
        .map_err(|source| ImageBuildError::Io {
            operation: "open image user home directory",
            path: directory.to_path_buf(),
            source,
        })?;
    fchown(&handle, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid))).map_err(|source| {
        ImageBuildError::Io {
            operation: "set image user home ownership",
            path: directory.to_path_buf(),
            source: io::Error::from_raw_os_error(source as i32),
        }
    })
}

pub(crate) fn normalized_image_id(image: &ImageId) -> Result<ImageId, ImageBuildError> {
    let mut hasher = Sha256::new();
    hasher.update(NORMALIZED_IMAGE_HASH_DOMAIN);
    hasher.update(image.as_str().as_bytes());
    ImageId::new(format!(
        "{NORMALIZED_IMAGE_ALGORITHM}:{:x}",
        hasher.finalize()
    ))
    .map_err(|error| {
        ImageBuildError::InvalidImageConfig(format!(
            "normalized image digest could not be represented: {error}"
        ))
    })
}

pub(crate) fn unsafe_account_database(path: &Path, reason: &'static str) -> ImageBuildError {
    ImageBuildError::UnsafeOutput {
        kind: "image account database",
        path: path.to_path_buf(),
        reason,
    }
}
