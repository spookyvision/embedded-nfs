use std::sync::Mutex;

use async_trait::async_trait;
use embedded_svc::wifi::{ClientConfiguration, Configuration};
use esp_idf_hal::prelude::Peripherals;
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    log::EspLogger,
    nvs::EspDefaultNvsPartition,
    timer::EspTaskTimerService,
    wifi::{AsyncWifi, EspWifi},
};
use esp_idf_sys as _;
use esp_idf_sys::{esp, esp_app_desc, EspError};
use log::info;
use nfsserve::{
    nfs::{
        self, fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, specdata3,
    },
    tcp::*,
    vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
// Edit these or provide your own way of provisioning...
const WIFI_SSID: Option<&str> = option_env!("WIFI_SSID");
const WIFI_PASS: Option<&str> = option_env!("WIFI_PASS");
const HOSTPORT: u32 = 111;

// To test, run `cargo run`, then when the server is up, use `nc -v espressif 12345` from
// a machine on the same Wi-Fi network.
const TCP_LISTENING_PORT: u16 = 12345;

esp_app_desc!();

fn main() -> anyhow::Result<()> {
    esp_idf_sys::link_patches();
    EspLogger::initialize_default();

    // eventfd is needed by our mio poll implementation.  Note you should set max_fds
    // higher if you have other code that may need eventfd.
    info!("Setting up eventfd...");
    let config = esp_idf_sys::esp_vfs_eventfd_config_t {
        max_fds: 1,
        ..Default::default()
    };
    esp! { unsafe { esp_idf_sys::esp_vfs_eventfd_register(&config) } }?;

    info!("Setting up board...");
    let peripherals = Peripherals::take().unwrap();
    let sysloop = EspSystemEventLoop::take()?;
    let timer = EspTaskTimerService::new()?;
    let nvs = EspDefaultNvsPartition::take()?;

    info!("Initializing Wi-Fi...");
    let wifi = AsyncWifi::wrap(
        EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs))?,
        sysloop,
        timer.clone(),
    )?;

    info!("Starting async run loop");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let mut wifi_loop = WifiLoop { wifi };
            wifi_loop.configure().await?;
            wifi_loop.initial_connect().await?;

            info!("Preparing to launch echo server...");
            tokio::spawn(echo_server());

            // info!("spawning NFS server…");
            // tokio::spawn(nfs_server());

            info!("Entering main Wi-Fi run loop...");
            wifi_loop.stay_connected().await
        })?;

    Ok(())
}

pub struct WifiLoop<'a> {
    wifi: AsyncWifi<EspWifi<'a>>,
}

impl<'a> WifiLoop<'a> {
    pub async fn configure(&mut self) -> Result<(), EspError> {
        info!("Setting Wi-Fi credentials...");
        self.wifi
            .set_configuration(&Configuration::Client(ClientConfiguration {
                ssid: WIFI_SSID
                    .expect("need env var: WIFI_SSID")
                    .try_into()
                    .unwrap(),
                password: WIFI_PASS
                    .expect("need env var: WIFI_PASS")
                    .try_into()
                    .unwrap(),
                ..Default::default()
            }))?;

        info!("Starting Wi-Fi driver...");
        self.wifi.start().await
    }

    pub async fn initial_connect(&mut self) -> Result<(), EspError> {
        self.do_connect_loop(true).await
    }

    pub async fn stay_connected(mut self) -> Result<(), EspError> {
        self.do_connect_loop(false).await
    }

    async fn do_connect_loop(&mut self, exit_after_first_connect: bool) -> Result<(), EspError> {
        let wifi = &mut self.wifi;
        loop {
            // Wait for disconnect before trying to connect again.  This loop ensures
            // we stay connected and is commonly missing from trivial examples as it's
            // way too difficult to showcase the core logic of an example and have
            // a proper Wi-Fi event loop without a robust async runtime.  Fortunately, we can do it
            // now!
            wifi.wifi_wait(|w| w.is_up(), None).await?;

            info!("Connecting to Wi-Fi...");
            wifi.connect().await?;

            info!("Waiting for association...");
            wifi.ip_wait_while(|w| w.is_up().map(|s| !s), None).await?;

            if exit_after_first_connect {
                return Ok(());
            }
        }
    }
}

async fn nfs_server() {
    let listener = NFSTcpListener::bind(&format!("0.0.0.0:{HOSTPORT}"), DemoFS::default())
        .await
        .unwrap();
    listener.handle_forever().await.unwrap();
}
// Test with
// mount -t nfs -o nolocks,vers=3,tcp,port=12000,mountport=12000,soft 127.0.0.1:/ mnt/

async fn echo_server() -> anyhow::Result<()> {
    let addr = format!("0.0.0.0:{TCP_LISTENING_PORT}");

    info!("Binding to {addr}...");
    let listener = TcpListener::bind(&addr).await?;

    loop {
        info!("Waiting for new connection on socket: {listener:?}");
        let (socket, _) = listener.accept().await?;

        info!("Spawning handle for: {socket:?}...");
        tokio::spawn(async move {
            info!("Spawned handler!");
            let peer = socket.peer_addr();
            if let Err(e) = serve_client(socket).await {
                info!("Got error handling {peer:?}: {e:?}");
            }
        });
    }
}

async fn serve_client(mut stream: TcpStream) -> anyhow::Result<()> {
    info!("Handling {stream:?}...");

    let mut buf = [0u8; 512];
    loop {
        info!("About to read...");
        let n = stream.read(&mut buf).await?;
        info!("Read {n} bytes...");

        if n == 0 {
            break;
        }

        stream.write_all(&buf[0..n]).await?;
        info!("Wrote {n} bytes back...");
    }

    Ok(())
}

#[derive(Debug, Clone)]
enum FSContents {
    File(Vec<u8>),
    Directory(Vec<fileid3>),
}
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct FSEntry {
    id: fileid3,
    attr: fattr3,
    name: filename3,
    parent: fileid3,
    contents: FSContents,
}

fn make_file(name: &str, id: fileid3, parent: fileid3, contents: &[u8]) -> FSEntry {
    let attr = fattr3 {
        ftype: ftype3::NF3REG,
        mode: 0o755,
        nlink: 1,
        uid: 507,
        gid: 507,
        size: contents.len() as u64,
        used: contents.len() as u64,
        rdev: specdata3::default(),
        fsid: 0,
        fileid: 2,
        atime: nfstime3::default(),
        mtime: nfstime3::default(),
        ctime: nfstime3::default(),
    };
    FSEntry {
        id,
        attr,
        name: name.as_bytes().into(),
        parent,
        contents: FSContents::File(contents.to_vec()),
    }
}

fn make_dir(name: &str, id: fileid3, parent: fileid3, contents: Vec<fileid3>) -> FSEntry {
    let attr = fattr3 {
        ftype: ftype3::NF3DIR,
        mode: 0o777,
        nlink: 1,
        uid: 507,
        gid: 507,
        size: 0,
        used: 0,
        rdev: specdata3::default(),
        fsid: 0,
        fileid: 1,
        atime: nfstime3::default(),
        mtime: nfstime3::default(),
        ctime: nfstime3::default(),
    };
    FSEntry {
        id,
        attr,
        name: name.as_bytes().into(),
        parent,
        contents: FSContents::Directory(contents),
    }
}

#[derive(Debug)]
pub struct DemoFS {
    fs: Mutex<Vec<FSEntry>>,
    rootdir: fileid3,
}

impl Default for DemoFS {
    fn default() -> DemoFS {
        // build the following directory structure
        // /
        // |-a.txt
        // |-b.txt
        // |-another_dir
        //      |-thisworks.txt
        //
        let entries = vec![
            make_file("", 0, 0, &[]), // fileid 0 is special
            make_dir(
                "/",
                1,             // current id. Must match position in entries
                1,             // parent id
                vec![2, 3, 4], // children
            ),
            make_file(
                "a.txt",
                2, // current id
                1, // parent id
                "hello world\n".as_bytes(),
            ),
            make_file("b.txt", 3, 1, "Greetings to xet data\n".as_bytes()),
            make_dir("another_dir", 4, 1, vec![5]),
            make_file("thisworks.txt", 5, 4, "i hope\n".as_bytes()),
        ];

        DemoFS {
            fs: Mutex::new(entries),
            rootdir: 1,
        }
    }
}

// For this demo file system we let the handle just be the file
// there is only 1 file. a.txt.
#[async_trait]
impl NFSFileSystem for DemoFS {
    fn root_dir(&self) -> fileid3 {
        self.rootdir
    }

    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        {
            let mut fs = self.fs.lock().unwrap();
            let mut fssize = fs[id as usize].attr.size;
            if let FSContents::File(bytes) = &mut fs[id as usize].contents {
                let offset = offset as usize;
                if offset + data.len() > bytes.len() {
                    bytes.resize(offset + data.len(), 0);
                    bytes[offset..].copy_from_slice(data);
                    fssize = bytes.len() as u64;
                }
            }
            fs[id as usize].attr.size = fssize;
            fs[id as usize].attr.used = fssize;
        }
        self.getattr(id).await
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        _attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let newid: fileid3;
        {
            let mut fs = self.fs.lock().unwrap();
            newid = fs.len() as fileid3;
            fs.push(make_file(
                std::str::from_utf8(filename).unwrap(),
                newid,
                dirid,
                "".as_bytes(),
            ));
            if let FSContents::Directory(dir) = &mut fs[dirid as usize].contents {
                dir.push(newid);
            }
        }
        Ok((newid, self.getattr(newid).await.unwrap()))
    }

    async fn create_exclusive(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let fs = self.fs.lock().unwrap();
        let entry = fs.get(dirid as usize).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if let FSContents::File(_) = entry.contents {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        } else if let FSContents::Directory(dir) = &entry.contents {
            // if looking for dir/. its the current directory
            if filename[..] == [b'.'] {
                return Ok(dirid);
            }
            // if looking for dir/.. its the parent directory
            if filename[..] == [b'.', b'.'] {
                return Ok(entry.parent);
            }
            for i in dir {
                if let Some(f) = fs.get(*i as usize) {
                    if f.name[..] == filename[..] {
                        return Ok(*i);
                    }
                }
            }
        }
        Err(nfsstat3::NFS3ERR_NOENT)
    }
    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let fs = self.fs.lock().unwrap();
        let entry = fs.get(id as usize).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        Ok(entry.attr)
    }
    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let mut fs = self.fs.lock().unwrap();
        let entry = fs.get_mut(id as usize).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        match setattr.atime {
            nfs::set_atime::DONT_CHANGE => {}
            nfs::set_atime::SET_TO_CLIENT_TIME(c) => {
                entry.attr.atime = c;
            }
            nfs::set_atime::SET_TO_SERVER_TIME => {
                // TODO
                log::warn!("unsupported: nfs::set_atime::SET_TO_SERVER_TIME");
                // let d = SystemTime::now()
                //     .duration_since(SystemTime::UNIX_EPOCH)
                //     .unwrap();
                // entry.attr.atime.seconds = d.as_secs() as u32;
                // entry.attr.atime.nseconds = d.subsec_nanos();
            }
        };
        match setattr.mtime {
            nfs::set_mtime::DONT_CHANGE => {}
            nfs::set_mtime::SET_TO_CLIENT_TIME(c) => {
                entry.attr.mtime = c;
            }
            nfs::set_mtime::SET_TO_SERVER_TIME => {
                // TODO
                log::warn!("unsupported: nfs::set_mtime::SET_TO_SERVER_TIME");
                // let d = SystemTime::now()
                //     .duration_since(SystemTime::UNIX_EPOCH)
                //     .unwrap();
                // entry.attr.mtime.seconds = d.as_secs() as u32;
                // entry.attr.mtime.nseconds = d.subsec_nanos();
            }
        };
        match setattr.uid {
            nfs::set_uid3::uid(u) => {
                entry.attr.uid = u;
            }
            nfs::set_uid3::Void => {}
        }
        match setattr.gid {
            nfs::set_gid3::gid(u) => {
                entry.attr.gid = u;
            }
            nfs::set_gid3::Void => {}
        }
        match setattr.size {
            nfs::set_size3::size(s) => {
                entry.attr.size = s;
                entry.attr.used = s;
                if let FSContents::File(bytes) = &mut entry.contents {
                    bytes.resize(s as usize, 0);
                }
            }
            nfs::set_size3::Void => {}
        }
        Ok(entry.attr)
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let fs = self.fs.lock().unwrap();
        let entry = fs.get(id as usize).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if let FSContents::Directory(_) = entry.contents {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        } else if let FSContents::File(bytes) = &entry.contents {
            let mut start = offset as usize;
            let mut end = offset as usize + count as usize;
            let eof = end >= bytes.len();
            if start >= bytes.len() {
                start = bytes.len();
            }
            if end > bytes.len() {
                end = bytes.len();
            }
            return Ok((bytes[start..end].to_vec(), eof));
        }
        Err(nfsstat3::NFS3ERR_NOENT)
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let fs = self.fs.lock().unwrap();
        let entry = fs.get(dirid as usize).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if let FSContents::File(_) = entry.contents {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        } else if let FSContents::Directory(dir) = &entry.contents {
            let mut ret = ReadDirResult {
                entries: Vec::new(),
                end: false,
            };
            let mut start_index = 0;
            if start_after > 0 {
                if let Some(pos) = dir.iter().position(|&r| r == start_after) {
                    start_index = pos + 1;
                } else {
                    return Err(nfsstat3::NFS3ERR_BAD_COOKIE);
                }
            }
            let remaining_length = dir.len() - start_index;

            for i in dir[start_index..].iter() {
                ret.entries.push(DirEntry {
                    fileid: *i,
                    name: fs[(*i) as usize].name.clone(),
                    attr: fs[(*i) as usize].attr,
                });
                if ret.entries.len() >= max_entries {
                    break;
                }
            }
            if ret.entries.len() == remaining_length {
                ret.end = true;
            }
            return Ok(ret);
        }
        Err(nfsstat3::NFS3ERR_NOENT)
    }

    /// Removes a file.
    /// If not supported dur to readonly file system
    /// this should return Err(nfsstat3::NFS3ERR_ROFS)
    #[allow(unused)]
    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        return Err(nfsstat3::NFS3ERR_NOTSUPP);
    }

    /// Removes a file.
    /// If not supported dur to readonly file system
    /// this should return Err(nfsstat3::NFS3ERR_ROFS)
    #[allow(unused)]
    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        return Err(nfsstat3::NFS3ERR_NOTSUPP);
    }

    #[allow(unused)]
    async fn mkdir(
        &self,
        _dirid: fileid3,
        _dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }
    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        return Err(nfsstat3::NFS3ERR_NOTSUPP);
    }
}
