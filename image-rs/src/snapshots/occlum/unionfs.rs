// Copyright (c) 2022 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

// This unionfs file is used for occlum only

use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::ffi::CString;

use anyhow::{anyhow, Context, Result};
use dircpy::CopyBuilder;
use fs_extra;
use fs_extra::dir;
use nix::mount::MsFlags;
use rand::Rng;

use ocicrypt_rs::blockcipher::rand::rand_bytes;

use crate::snapshots::{MountPoint, Snapshotter};

const LD_LIB: &str = "ld-linux-x86-64.so.2";

#[derive(Debug)]
pub struct Unionfs {
    pub data_dir: PathBuf,
    pub index: AtomicUsize,
}

fn clear_path(mount_path: &Path) -> Result<()> {
    let mut from_paths = Vec::new();
    let paths = fs::read_dir(
        mount_path
            .to_str()
            .ok_or(anyhow!("mount_path does not exist"))?,
    )?;
    for path in paths {
        from_paths.push(path?.path());
    }
    fs_extra::remove_items(&from_paths)?;

    Ok(())
}

fn create_dir(create_path: &Path) -> Result<()> {
    if !create_path.exists() {
        fs::create_dir_all(create_path)?;
    }

    Ok(())
}

// returns randomly generted random 128 bit key
fn generate_random_key() -> String {

    let mut key: [u8; 16] = [0u8; 16];

    rand_bytes(&mut key).expect("Random fill failed");

    let formatted_key = key.iter().map(|byte| format!("{:02x}", byte)).collect::<Vec<String>>().join("-");

    formatted_key
}
fn create_key_file(path: &PathBuf, key: &str) -> Result<()> {
    let mut file = File::create(path)
        .with_context(|| format!("Failed to create file: {:?}", path))?;

    file.write_all(key.as_bytes())
        .with_context(|| format!("Failed to write to file: {:?}", path))?;

    Ok(())

}

fn create_environment(mount_path: &Path) -> Result<()> {
    let mut from_paths = Vec::new();
    let mut copy_options = dir::CopyOptions::new();
    copy_options.overwrite = true;

    // copy the libs required by occlum to the mount path
    let path_lib64 = mount_path.join("lib64");
    create_dir(&path_lib64)?;

    let lib64_libs = [LD_LIB];
    let ori_path_lib64 = Path::new("/lib64");
    for lib in lib64_libs.iter() {
        from_paths.push(ori_path_lib64.join(lib));
    }

    // if ld-linux-x86-64.so.2 as symlink exist in ${path_lib64},
    // copy ld-linux-x86-64.so.2 from occlum to ${path_lib64} failed (file exists).
    // so firstly remove it.
    let ld_lib = path_lib64.join(LD_LIB);
    if fs::symlink_metadata(ld_lib.as_path()).is_ok() {
        fs::remove_file(ld_lib)?;
    }
    fs_extra::copy_items(&from_paths, &path_lib64, &copy_options)?;
    from_paths.clear();

    let path_opt = mount_path
        .join("opt")
        .join("occlum")
        .join("glibc")
        .join("lib");
    fs::create_dir_all(&path_opt)?;

    let occlum_lib = [
        "libc.so.6",
        "libdl.so.2",
        "libm.so.6",
        "libpthread.so.0",
        "libresolv.so.2",
        "librt.so.1",
    ];

    let ori_occlum_lib_path = Path::new("/")
        .join("opt")
        .join("occlum")
        .join("glibc")
        .join("lib");
    for lib in occlum_lib.iter() {
        from_paths.push(ori_occlum_lib_path.join(lib));
    }
    fs_extra::copy_items(&from_paths, &path_opt, &copy_options)?;
    from_paths.clear();

    let sys_path = ["dev", "etc", "host", "lib", "proc", "root", "sys", "tmp"];
    for path in sys_path.iter() {
        create_dir(&mount_path.join(path))?;
    }

    Ok(())
}

impl Snapshotter for Unionfs {
    fn mount(&mut self, layer_path: &[&str], mount_path: &Path) -> Result<MountPoint> {
        let fs_type = String::from("sefs");
        let source = Path::new(&fs_type);

        if !mount_path.exists() {
            fs::create_dir_all(mount_path)?;
        }

        // store the rootfs in different places according to the cid
        let cid = mount_path
            .parent()
            .ok_or(anyhow!("parent do not exist"))?
            .file_name()
            .ok_or(anyhow!("Unknown error: file name parse fail"))?;

        let random_key = generate_random_key();
        let new_keys_path = Path::new("/new_key").join(cid);
        fs::create_dir_all(&new_keys_path)?;
        create_key_file(
            &PathBuf::from(&new_keys_path.join("key.txt")), 
            &random_key)
        .map_err(|e| {
            anyhow!(
                "failed to write key file {:?} with error: {}",
                "/key.txt",
                e
            )
        })?;

        let options_2 = format!(
            "dir={}",
            Path::new("/images").join(cid).join("keys/sefs/lower").display()
        );


        let flags = MsFlags::empty();

        let keys_mount_path = Path::new("/keys");
        println!("{:#?} {:#?} {:#?} {:#?} {:#?}", source, keys_mount_path, fs_type, flags, options_2.as_str());
        nix::mount::mount(
            Some(source), //"sefs"
            keys_mount_path,
            Some(fs_type.as_str()), //"sefs"
            flags,
            Some(options_2.as_str()),
        )
        .map_err(|e| {
            anyhow!(
                "failed to mount {:?} to {:?}, with error: {}",
                source,
                keys_mount_path,
                e
            )
        })?;
        let mut copy_options = dir::CopyOptions::new();
        copy_options.overwrite = true;
        let paths = fs::read_dir("/new_key").unwrap();

        for entry in paths {
            let path = entry.unwrap().path();
            println!("Name: {}", path.display());
            dir::copy(path, "/keys", &copy_options).unwrap();
        }

        println!("List directories in {:#?}", keys_mount_path);
        let paths = fs::read_dir("/keys").unwrap();

        for path in paths {
            println!("Name: {}", path.unwrap().path().display())
        }


        // match fs::copy("/new_key/key.txt", "/keys/scratch-base_v1.8/key.txt") {
        //     Ok(_) => println!("File copied successfully"),
        //     Err(e) => println!("Failed to copy file: {}", e),
        // }

        // println!("clear path {:?}", keys_mount_path);
        // clear_path(keys_mount_path)?;
        println!("Unmount {:?}", keys_mount_path);
        nix::mount::umount(keys_mount_path)?;


        let options = format!(
            "dir={},key={}",
            Path::new("/images").join(cid).join("sefs/lower").display(),
            "c7-32-b3-ed-44-df-ec-7b-25-2d-9a-32-38-8d-58-61"
        );

        println!("{:#?} {:#?} {:#?} {:#?} {:#?}", source, mount_path, fs_type, flags, options.as_str());
        nix::mount::mount(
            Some(source),
            mount_path, // /run/rune/Occlum_instance
            Some(fs_type.as_str()),
            flags,
            Some(options.as_str()),
        )
            .map_err(|e| {
                anyhow!(
                "failed to mount {:?} to {:?}, with error: {}",
                source,
                mount_path,
                e
            )
            })?;

        // clear the mount_path if there is something
        clear_path(mount_path)?;

        // copy dirs to the specified mount directory
        let mut layer_path_vec = layer_path.to_vec();
        let len = layer_path_vec.len();
        for _i in 0..len {
            let layer = layer_path_vec
                .pop()
                .ok_or(anyhow!("Pop() failed from Vec"))?;
            CopyBuilder::new(layer, mount_path).overwrite(true).run()?;
        }

        // create environment for Occlum
        create_environment(mount_path)?;

        nix::mount::umount(mount_path)?;

        Ok(MountPoint {
            r#type: fs_type,
            mount_path: mount_path.to_path_buf(),
            work_dir: self.data_dir.to_path_buf(),
        })
    }

    fn unmount(&self, mount_point: &MountPoint) -> Result<()> {
        nix::mount::umount(mount_point.mount_path.as_path())?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile;

    #[test]
    fn test_create_dir() {
        let tempdir = tempfile::tempdir().unwrap();

        let foo_dir = tempdir.path().join("foo");
        assert!(!foo_dir.exists());

        create_dir(&foo_dir).unwrap();
        assert!(foo_dir.exists());
    }

    #[test]
    fn test_clear_path() {
        let tempdir = tempfile::tempdir().unwrap();

        let file = tempdir.path().join("foo.txt");
        let mut f = File::create(file.as_path()).unwrap();
        f.write_all(b"Hello, world!").unwrap();
        assert!(file.exists());

        clear_path(tempdir.path()).unwrap();
        assert!(!file.exists());
    }

    #[allow(unused_macros)]
    macro_rules! skip_if_root {
        () => {
            if nix::unistd::Uid::effective().is_root() {
                println!("INFO: skipping {} which needs non-root", module_path!());
                return;
            }
        };
    }

    #[allow(unused_macros)]
    macro_rules! skip_if_not_root {
        () => {
            if !nix::unistd::Uid::effective().is_root() {
                println!("INFO: skipping {} which needs root", module_path!());
                return;
            }
        };
    }

    #[test]
    fn test_mount() {
        skip_if_root!();

        let mnt_path = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap().path().to_path_buf();

        let unionfs_index = 0;
        let mut occlum_unionfs = Unionfs {
            data_dir: work_dir,
            index: AtomicUsize::new(unionfs_index),
        };

        let path_1 = tempfile::tempdir().unwrap();
        let path_2 = tempfile::tempdir().unwrap();
        let layer_path = &[
            path_1.path().to_str().unwrap(),
            path_2.path().to_str().unwrap(),
        ];

        assert!(occlum_unionfs.mount(layer_path, mnt_path.as_ref()).is_err());
    }
}