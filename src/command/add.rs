use crate::util;
use byteorder::{BigEndian, ByteOrder};
use core::panic;
use hex;
use std::collections;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::linux::fs::MetadataExt;

struct IndexHeader {
    signature: [u8; 4],
    version: [u8; 4],
    entries: [u8; 4],
}

struct IndexEntry {
    ctime: [u8; 4],
    ctime_nsec: [u8; 4],
    mtime: [u8; 4],
    mtime_nsec: [u8; 4],
    dev: [u8; 4],
    ino: [u8; 4],
    mode: [u8; 4],
    uid: [u8; 4],
    gid: [u8; 4],
    file_size: [u8; 4],
    oid: String, // 20byte
    flags: [u8; 2],
    path: String,
}

fn travel_dir(file_name: &String, file_path_list: &mut Vec<String>, hash_list: &mut Vec<String>) {
    if fs::metadata(file_name).unwrap().is_dir() {
        // 再帰的にaddする
        for entry in fs::read_dir(file_name).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.starts_with("./.git") {
                continue;
            }
            if path.is_dir() {
                travel_dir(
                    &path.to_str().unwrap().to_string(),
                    file_path_list,
                    hash_list,
                );
                continue;
            }
            let file_name = path.to_str().unwrap().to_string();
            let hash = generate_blob_object(&file_name);
            file_path_list.push(file_name);
            hash_list.push(hash);
        }
    } else {
        let hash = generate_blob_object(file_name);
        file_path_list.push(file_name.clone());
        hash_list.push(hash);
    }
}

pub fn add(file_names: &[String]) {
    let mut hash_list = Vec::new();
    let mut file_path_list = Vec::new();
    for file_name in file_names {
        travel_dir(file_name, &mut file_path_list, &mut hash_list);
    }
    update_index(&file_path_list, hash_list);
}

fn generate_blob_object(file_name: &String) -> String {
    let contents = fs::read_to_string(file_name).unwrap();
    let file_length = contents.len();

    // データの準備
    let header = format!("blob {}\0", file_length);
    let hash = util::compress::hash(&format!("{}{}", header, contents).as_bytes());

    // ファイルの準備
    let file_directory = format!(".git/objects/{}", &hash[0..2]);
    let file_path = format!("{}/{}", file_directory, &hash[2..]);
    std::fs::create_dir_all(file_directory).unwrap();
    let mut file = File::create(file_path).unwrap();

    // zlib圧縮
    let contents_will_be_compressed = format!("{}{}", header, contents);
    let compressed_contents =
        util::compress::zlib_compress(&contents_will_be_compressed.as_bytes());

    // ファイルに書き込み
    file.write_all(&compressed_contents).unwrap();

    hash
}

#[derive(Clone)]
struct IndexEntrySummary {
    index_entry: Vec<u8>,
    path: String,
}

// 既存のentriesと新しく追加されるentriesをmergeする
// 順番を変えるとファイルが削除されて新しく作成されたとみなされてしまうため、順番は変わらないようにする
fn merge_entries(
    exists: Vec<IndexEntrySummary>,
    new_entries: Vec<IndexEntrySummary>,
) -> Vec<IndexEntrySummary> {
    let exist_paths: collections::HashSet<_> = exists.iter().map(|x| x.path.clone()).collect();
    let new_paths: collections::HashSet<_> = new_entries.iter().map(|x| x.path.clone()).collect();

    let common_paths: collections::HashSet<_> = exist_paths.intersection(&new_paths).collect();

    let mut result = Vec::<IndexEntrySummary>::new();

    for entry in exists.to_vec() {
        if !common_paths.contains(&entry.path) {
            result.push(entry);
        } else {
            match new_entries
                .to_vec()
                .into_iter()
                .find(|x| x.path == entry.path)
            {
                Some(item) => {
                    let new_entry = IndexEntrySummary {
                        index_entry: item.index_entry.clone(),
                        path: item.path.clone(),
                    };
                    result.push(new_entry);
                }
                None => panic!("not found"),
            };
        }
    }
    for entry in new_entries {
        if !common_paths.contains(&entry.path) {
            result.push(entry);
        }
    }

    result
}

fn decode_index_file() -> Option<Vec<IndexEntrySummary>> {
    let mut file = match File::open(".git/index") {
        Ok(file) => file,
        Err(_) => return None,
    };
    let mut content = Vec::new();
    let mut index_entry_summaries = Vec::<IndexEntrySummary>::new();
    file.read_to_end(&mut content).unwrap();

    // entriesを上から1 entryずつ消費していく
    let entry_count = BigEndian::read_u32(&content[8..12]);
    let mut entries = &content[12..];
    for _ in 0..entry_count {
        let (next_byte, index_entry_summary) = decode_index_entry(&entries);
        index_entry_summaries.push(index_entry_summary);
        entries = &entries[next_byte..];
    }

    Some(index_entry_summaries)
}

fn decode_index_entry(entry: &[u8]) -> (usize, IndexEntrySummary) {
    let flags = BigEndian::read_u16(&entry[60..62]);
    let file_path_end_byte = (62 + flags) as usize;
    let path = std::str::from_utf8(&entry[62..file_path_end_byte]).unwrap();

    let padding = 4 - (file_path_end_byte % 4);
    let next_byte = file_path_end_byte + padding;
    let index_entry_summary = IndexEntrySummary {
        index_entry: entry[..next_byte].to_vec(),
        path: path.to_string(),
    };

    (next_byte, index_entry_summary)
}

fn update_index(file_names: &[String], hash_list: Vec<String>) {
    // 既にindex fileが存在したらそれを読み込み、entriesをdecode
    // headerは新しく作る(entryの数が違うため)

    // 更新されるファイルのentries
    let exists = decode_index_file();

    // 新しく追加されるファイルのentries
    let mut new_entries = Vec::<IndexEntrySummary>::new();

    for (index, file_name) in file_names.iter().enumerate() {
        let mut content: Vec<u8> = Vec::new();
        let metadata = fs::metadata(file_name).unwrap();

        let new_file_name = match file_name.strip_prefix("./") {
            Some(file_name) => file_name,
            None => file_name,
        };
        let index_entry = IndexEntry {
            ctime: metadata.st_ctime().to_be_bytes()[4..8].try_into().unwrap(),
            ctime_nsec: metadata.st_ctime_nsec().to_be_bytes()[4..8]
                .try_into()
                .unwrap(),
            mtime: metadata.st_mtime().to_be_bytes()[4..8].try_into().unwrap(),
            mtime_nsec: metadata.st_mtime_nsec().to_be_bytes()[4..8]
                .try_into()
                .unwrap(),
            dev: metadata.st_dev().to_be_bytes()[4..8].try_into().unwrap(),
            ino: metadata.st_ino().to_be_bytes()[4..8].try_into().unwrap(),
            mode: metadata.st_mode().to_be_bytes(),
            uid: metadata.st_uid().to_be_bytes(),
            gid: metadata.st_gid().to_be_bytes(),
            file_size: metadata.st_size().to_be_bytes()[4..8].try_into().unwrap(),
            oid: hash_list[index].clone(),
            // TODO: 正しく計算
            flags: new_file_name.len().to_be_bytes()[6..8].try_into().unwrap(),
            path: new_file_name.to_string(),
        };

        content.extend(index_entry.ctime.to_vec());
        content.extend(index_entry.ctime_nsec.to_vec());
        content.extend(index_entry.mtime.to_vec());
        content.extend(index_entry.mtime_nsec.to_vec());
        content.extend(index_entry.dev.to_vec());
        content.extend(index_entry.ino.to_vec());
        content.extend(index_entry.mode.to_vec());
        content.extend(index_entry.uid.to_vec());
        content.extend(index_entry.gid.to_vec());
        content.extend(index_entry.file_size.to_vec());
        let decoded_oid = hex::decode(index_entry.oid.clone()).unwrap();
        content.extend(decoded_oid);
        content.extend(index_entry.flags.to_vec());
        content.extend(index_entry.path.as_bytes().to_vec());
        let padding = 4 - (content.len() % 4);
        for _ in 0..padding {
            content.push(0);
        }

        let index_entry_summary = IndexEntrySummary {
            index_entry: content.clone(),
            path: index_entry.path.to_string(),
        };
        new_entries.push(index_entry_summary);
    }

    let merged_entries = match exists {
        Some(exists) => merge_entries(exists, new_entries),
        None => new_entries,
    };

    let mut contents: Vec<u8> = Vec::new();
    // header
    let index_header = IndexHeader {
        signature: "DIRC".as_bytes().try_into().unwrap(),
        version: 2u32.to_be_bytes(),
        entries: merged_entries.len().to_be_bytes()[4..8].try_into().unwrap(),
    };
    contents.extend(index_header.signature.to_vec());
    contents.extend(index_header.version.to_vec());
    contents.extend(index_header.entries.to_vec());

    // entries
    for entry in merged_entries {
        contents.extend(entry.index_entry);
    }

    let mut file = File::create(".git/index").unwrap();
    file.write_all(&contents).unwrap();
}
