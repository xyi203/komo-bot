//! `komo update`：从 GitHub release 换掉正在跑的这个可执行文件（§3、§13.6）。
//!
//! 顺序是「下载 → 校验 sha256 → 解包 → 试跑 `--version` → 同目录 rename 换上去」。
//! **换上去是最后一步，而且是 `rename`**：中途任何一步失败，现在装着的那个 komo 一个
//! 字节都没动。自我更新唯一不可接受的结局不是"没更新成"，是"更新成一个跑不起来的东西"，
//! 所以校验和试跑都赶在旧文件还在的时候做。
//!
//! 资产名（`komo-<os>-<arch>.tar.gz`）、校验和文件名（`SHA256SUMS`）与仓库名是三处共同
//! 约定：这里、`.github/workflows/release.yml`、`install.sh`。改一处就得改另外两处。

use std::path::Path;
use std::time::Duration;

use komo_client::discovery;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// 发布仓库；`install.sh` 的默认值必须与它一致。
const REPO: &str = "xyi203/komo-bot";
/// 发布包里唯一的成员。
const BINARY: &str = "komo";
/// 所有资产共用一份校验和，格式就是 `sha256sum` 的输出。
const SUMS: &str = "SHA256SUMS";
/// 连上 GitHub 的时限。**不设整体超时**：包二十来兆，链路慢的时候整体超时会把一次正常
/// 的下载砍断；真正卡住的连接由 `read_timeout` 兜。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// `komo update`。
pub async fn run(home: &Path) -> Result<String, String> {
    // 符号链接要落到真身上：装的时候 `/usr/local/bin/komo` 可能是指向别处的软链，
    // 换掉软链本身等于换掉一个链接文件，不是换掉可执行文件。
    let dest = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .map_err(|error| format!("找不到自己这个可执行文件在哪：{error}"))?;
    let api = format!("https://api.github.com/repos/{REPO}/releases/latest");
    update_against(home, &client()?, &api, &dest).await
}

/// 换的这一步。`api` 与 `dest` 是参数、不是常量，测试里指向假服务端与临时文件
/// （渠道那边同一种形状：假服务端靠参数进来，不靠全局开关）。
async fn update_against(
    home: &Path,
    client: &reqwest::Client,
    api: &str,
    dest: &Path,
) -> Result<String, String> {
    let release = latest(client, api).await?;
    let tag = release.tag_name.trim();
    let published = version(tag)
        .ok_or_else(|| format!("发布标签认不出：{tag}（这里认 v<major>.<minor>.<patch>）"))?;
    let installed = version(env!("CARGO_PKG_VERSION")).expect("Cargo.toml 里的版本号");

    if published < installed {
        return Ok(format!(
            "现在是 {}，最新发布是 {tag}——发布的比它旧，没动。",
            env!("CARGO_PKG_VERSION")
        ));
    }
    if published == installed {
        return Ok(format!("已经是最新：{tag}（{}）。", dest.display()));
    }

    let asset = format!("{BINARY}-{}.tar.gz", asset_suffix()?);
    install(client, &release, &asset, tag, dest).await?;
    Ok(format!(
        "komo {} → {tag}，已换到 {}。{}",
        env!("CARGO_PKG_VERSION"),
        dest.display(),
        live_gateway_hint(home).await
    ))
}

/// 换二进制不等于换正在跑的那个进程。**只报告，不替你重启**：重启会打断正在跑的 Run
/// 和等在那里的审批，那不该由一条更新命令决定。
async fn live_gateway_hint(home: &Path) -> &'static str {
    // 只凭发现文件在不在判断不了（§3 第 2 步）——它可能早就过期了，所以走一遍健康核对。
    if discovery::discover(home).await.is_ok() {
        " Gateway 还在跑旧的那一份，`komo gateway restart` 之后才生效。"
    } else {
        ""
    }
}

/// 下载、校验、解包、试跑到 `staged`，然后一次 `rename` 换上去。
///
/// `dest` 只在最后那一下被碰到——这是这个模块的不变量，写在这里而不是散在 `stage` 里。
async fn install(
    client: &reqwest::Client,
    release: &Release,
    asset: &str,
    tag: &str,
    dest: &Path,
) -> Result<(), String> {
    let directory = dest
        .parent()
        .ok_or_else(|| format!("{} 没有所在目录", dest.display()))?;
    // 临时文件必须与 dest 同目录：跨文件系统的 `rename` 会退化成复制，而复制到一半
    // 断电就留下一个半个二进制。带 pid 是为了两次更新撞在一起时各写各的。
    let stamp = std::process::id();
    let archive = directory.join(format!(".komo-{stamp}.tar.gz"));
    let staged = directory.join(format!(".komo-{stamp}.new"));

    let outcome = stage(client, release, asset, tag, &archive, &staged).await;
    let _ = std::fs::remove_file(&archive);
    match outcome {
        Ok(()) => std::fs::rename(&staged, dest).map_err(|error| {
            format!(
                "换不上去（{} → {}）：{error}。看看 {} 你能不能写",
                staged.display(),
                dest.display(),
                directory.display()
            )
        }),
        Err(error) => {
            let _ = std::fs::remove_file(&staged);
            Err(error)
        }
    }
}

async fn stage(
    client: &reqwest::Client,
    release: &Release,
    asset: &str,
    tag: &str,
    archive: &Path,
    staged: &Path,
) -> Result<(), String> {
    download(client, release.asset(asset)?, archive).await?;

    let sums = client_text(client, release.asset(SUMS)?).await?;
    let expected = sum_for(&sums, asset)
        .ok_or_else(|| format!("{SUMS} 里没有 {asset} 那一行，没敢动现在这份"))?;
    let actual = sha256_hex(archive)?;
    if actual != expected {
        return Err(format!(
            "{asset} 的 sha256 是 {actual}，{SUMS} 说 {expected}——下到的东西不对，没有换上去"
        ));
    }

    extract(archive, staged)?;
    set_executable(staged)?;
    probe(staged, tag)
}

// ---------------------------------------------------------------- GitHub

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

impl Release {
    /// 地址从**响应里**取，不自己拼 URL：拼错了只有 404，而这里能直接说出"这一版没有
    /// 你这个平台的包"。
    fn asset(&self, name: &str) -> Result<&str, String> {
        self.assets
            .iter()
            .find(|asset| asset.name == name)
            .map(|asset| asset.browser_download_url.as_str())
            .ok_or_else(|| {
                let names: Vec<&str> = self
                    .assets
                    .iter()
                    .map(|asset| asset.name.as_str())
                    .collect();
                format!(
                    "{} 里没有 {name}（它只有：{}）",
                    self.tag_name,
                    names.join("、")
                )
            })
    }
}

/// **User-Agent 是必需的**：GitHub 的 API 对不带它的请求回 403。
fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("komo/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .map_err(|error| format!("建 HTTP 客户端失败：{error}"))
}

async fn latest(client: &reqwest::Client, api: &str) -> Result<Release, String> {
    get(client, api)
        .await?
        .json()
        .await
        .map_err(|error| format!("{api} 的响应读不出来：{error}"))
}

async fn client_text(client: &reqwest::Client, url: &str) -> Result<String, String> {
    get(client, url)
        .await?
        .text()
        .await
        .map_err(|error| format!("{url} 读不出来：{error}"))
}

async fn get(client: &reqwest::Client, url: &str) -> Result<reqwest::Response, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("请求 {url} 失败：{error}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(match status.as_u16() {
        404 => format!("{url} 回 404：仓库或 release 不存在"),
        403 => format!(
            "{url} 回 403：多半是 GitHub 的限流（匿名 60 次/小时），过一会儿再试。{}",
            snippet(&body)
        ),
        _ => format!("{url} 回 {status}：{}", snippet(&body)),
    })
}

fn snippet(text: &str) -> String {
    text.chars().take(200).collect()
}

// ---------------------------------------------------------------- 平台与版本

/// 当前平台对应的资产后缀，与 `release.yml` 的矩阵一一对应。
fn asset_suffix() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("darwin-arm64"),
        ("macos", "x86_64") => Ok("darwin-amd64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        ("linux", "x86_64") => Ok("linux-amd64"),
        (os, arch) => Err(format!(
            "发布包里有 darwin / linux 的 arm64 与 amd64，没有 {os}-{arch}——\
             这一份得自己 `cargo build --release`"
        )),
    }
}

/// `v0.10.0` / `0.10.0` → `(0, 10, 0)`。认不出就是 `None`：发布标签是
/// `v<major>.<minor>.<patch>`，这里不猜版本号。
fn version(tag: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag
        .trim()
        .strip_prefix('v')
        .unwrap_or(tag.trim())
        .split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    match parts.next() {
        None => Some((major, minor, patch)),
        Some(_) => None,
    }
}

// ---------------------------------------------------------------- 校验与解包

async fn download(client: &reqwest::Client, url: &str, dest: &Path) -> Result<(), String> {
    let mut response = get(client, url).await?;
    let mut file = tokio::fs::File::create(dest).await.map_err(|error| {
        // 最常见的失败：装到 /usr/local/bin 这类 root 才写得进的目录。临时文件必须落在
        // 目标旁边（同文件系统才换得动），所以这里没得退让，只能把出路说清楚。
        match error.kind() {
            std::io::ErrorKind::PermissionDenied => format!(
                "写不进 {}：{error}——换二进制要往它旁边写临时文件。\
                 装到你有写权限的目录（`install.sh` 默认就是 `~/.local/bin`），或者用 sudo 跑这一条",
                dest.display()
            ),
            _ => format!("建不了 {}：{error}", dest.display()),
        }
    })?;
    // 边下边写：包二十来兆，攒在内存里再落盘没有好处。
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("从 {url} 下到一半断了：{error}"))?
    {
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("写 {} 失败：{error}", dest.display()))?;
    }
    file.flush()
        .await
        .map_err(|error| format!("写 {} 失败：{error}", dest.display()))
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("读不了 {}：{error}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|error| format!("算 {} 的 sha256 失败：{error}", path.display()))?;
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `SHA256SUMS` 是标准的 `sha256sum` 输出：一行 `<hex>  <文件名>`。
fn sum_for(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let digest = fields.next()?;
        let name = fields.next()?;
        (name == asset).then(|| digest.to_ascii_lowercase())
    })
}

/// 从发布包里取出 `komo`。**只认这一个名字**：发布包是我们自己打的
/// （`tar -C dist komo`），按包里的路径往任意位置解包等于把发布包变成一条写任意文件的
/// 路。
fn extract(archive: &Path, into: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive)
        .map_err(|error| format!("读不了 {}：{error}", archive.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let entries = archive
        .entries()
        .map_err(|error| format!("发布包读不动：{error}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("发布包坏了：{error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("发布包里的路径读不出来：{error}"))?;
        if path != Path::new(BINARY) {
            continue;
        }
        let mut out = std::fs::File::create(into)
            .map_err(|error| format!("建不了 {}：{error}", into.display()))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|error| format!("解包到 {} 失败：{error}", into.display()))?;
        return Ok(());
    }
    Err(format!("发布包里没有 {BINARY}"))
}

fn set_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 不沿用包里的模式位：这个文件的权限由这里给死。
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("给 {} 加执行位失败：{error}", path.display()))?;
    }
    let _ = path;
    Ok(())
}

/// 试跑一次。**新的那份真能在这台机器上跑起来、报的版本就是我们要的那个吗**——glibc
/// 太旧、架构不对、包下坏了，都在这里挡下来，此时 `dest` 还是原来那一份。
fn probe(staged: &Path, tag: &str) -> Result<(), String> {
    let output = std::process::Command::new(staged)
        .arg("--version")
        .output()
        .map_err(|error| format!("试跑 {} 失败：{error}", staged.display()))?;
    let printed = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() {
        return Err(format!(
            "下下来的那份跑不起来（{}，它说「{printed}」）——没有换上去",
            output.status
        ));
    }
    let wanted = tag.strip_prefix('v').unwrap_or(tag);
    if !printed.contains(wanted) {
        return Err(format!(
            "下下来的那份报的是「{printed}」，不是这一版的 {tag}——发布包与标签对不上，没有换上去"
        ));
    }
    Ok(())
}

/// 目录里 `komo update` 留下的临时文件（测试断言"收干净了"时用得到）。
#[cfg(test)]
fn dotfiles(directory: &Path) -> Vec<std::path::PathBuf> {
    let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(directory)
        .expect("读目录")
        .map(|entry| entry.expect("目录项").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".komo-"))
        })
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::{Path as UrlPath, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use serde_json::json;
    use tokio::task::JoinHandle;

    /// 一个假 GitHub：`/latest` 给一份 release 报文，`/files/<名字>` 给资产本体。
    struct Fake {
        addr: SocketAddr,
        task: JoinHandle<()>,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[derive(Clone)]
    struct FakeState {
        files: Arc<Vec<(String, Vec<u8>)>>,
        manifest: Arc<serde_json::Value>,
    }

    async fn serve_manifest(State(state): State<FakeState>) -> axum::Json<serde_json::Value> {
        axum::Json((*state.manifest).clone())
    }

    async fn serve_file(
        State(state): State<FakeState>,
        UrlPath(name): UrlPath<String>,
    ) -> Response {
        match state.files.iter().find(|(file, _)| *file == name) {
            Some((_, bytes)) => Body::from(bytes.clone()).into_response(),
            None => (StatusCode::NOT_FOUND, "no such asset").into_response(),
        }
    }

    impl Fake {
        async fn start(tag: &str, files: Vec<(String, Vec<u8>)>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("绑定 loopback");
            let addr = listener.local_addr().expect("本地地址");
            let manifest = json!({
                "tag_name": tag,
                "assets": files
                    .iter()
                    .map(|(name, _)| json!({
                        "name": name,
                        "browser_download_url": format!("http://{addr}/files/{name}"),
                    }))
                    .collect::<Vec<_>>(),
            });
            let app = Router::new()
                .route("/latest", get(serve_manifest))
                .route("/files/{name}", get(serve_file))
                .with_state(FakeState {
                    files: Arc::new(files),
                    manifest: Arc::new(manifest),
                });
            let task = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self { addr, task }
        }

        fn api(&self) -> String {
            format!("http://{}/latest", self.addr)
        }
    }

    /// 一份"发布包"：一个 tar.gz，里面只有一个名叫 `komo` 的可执行文件。
    fn release_archive(binary: &str) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(binary.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, BINARY, binary.as_bytes())
            .expect("打包");
        builder.into_inner().expect("收尾").finish().expect("压缩")
    }

    /// `sha256sum` 那两列的原文。
    fn sums_for(asset: &str, archive: &[u8]) -> String {
        format!("{}  {asset}\n", hex(&Sha256::digest(archive)))
    }

    /// 一份真的会打印自己版本的"komo"。
    fn fake_binary(tag: &str) -> String {
        format!("#!/bin/sh\necho komo {}\n", tag.trim_start_matches('v'))
    }

    fn unchanged(dest: &Path, was: &str) -> bool {
        std::fs::read_to_string(dest).expect("读现在这份") == was
    }

    #[tokio::test]
    async fn a_newer_release_is_swapped_in_place() {
        let tag = "v99.0.0";
        let archive = release_archive(&fake_binary(tag));
        let asset = format!("{BINARY}-{}.tar.gz", asset_suffix().expect("本机平台"));
        let fake = Fake::start(
            tag,
            vec![
                (asset.clone(), archive.clone()),
                (SUMS.to_string(), sums_for(&asset, &archive).into_bytes()),
            ],
        )
        .await;

        let dir = tempfile::tempdir().expect("临时目录");
        // 现在装的那一份：一个旧内容 + 0755，换完必须真的被替换掉。
        let dest = dir.path().join(BINARY);
        std::fs::write(&dest, "旧的").expect("铺一个现在装着的");

        let home = tempfile::tempdir().expect("临时 home");
        let printed = update_against(home.path(), &client().expect("客户端"), &fake.api(), &dest)
            .await
            .expect("换得上");

        assert!(printed.contains("99.0.0"), "{printed}");
        assert_eq!(
            std::fs::read_to_string(&dest).expect("读新的"),
            fake_binary(tag),
            "dest 要真的变成新下下来的那一份"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest)
                .expect("元数据")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "换上去的那份要能执行");
        }
        assert!(
            dotfiles(dir.path()).is_empty(),
            "临时文件要收干净：{:?}",
            dotfiles(dir.path())
        );
    }

    /// 校验和对不上时，**现在这份一个字节都不能动**——这是整条命令存在的理由。
    #[tokio::test]
    async fn a_bad_checksum_leaves_the_installed_binary_alone() {
        let tag = "v99.0.0";
        let archive = release_archive(&fake_binary(tag));
        let asset = format!("{BINARY}-{}.tar.gz", asset_suffix().expect("本机平台"));
        let fake = Fake::start(
            tag,
            vec![
                (asset.clone(), archive),
                (
                    SUMS.to_string(),
                    format!("{}  {asset}\n", "0".repeat(64)).into_bytes(),
                ),
            ],
        )
        .await;

        let dir = tempfile::tempdir().expect("临时目录");
        let dest = dir.path().join(BINARY);
        std::fs::write(&dest, "旧的").expect("铺一个现在装着的");
        let home = tempfile::tempdir().expect("临时 home");

        let error = update_against(home.path(), &client().expect("客户端"), &fake.api(), &dest)
            .await
            .expect_err("校验和不对就该拒绝");

        assert!(error.contains("sha256"), "{error}");
        assert!(unchanged(&dest, "旧的"), "失败时不许碰 dest");
        assert!(
            dotfiles(dir.path()).is_empty(),
            "{:?}",
            dotfiles(dir.path())
        );
    }

    /// 包里的那份跑不起来（架构不对、glibc 太旧、包被改过）时，旧的还在。
    #[tokio::test]
    async fn a_binary_that_does_not_run_is_not_swapped_in() {
        let tag = "v99.0.0";
        let archive = release_archive("#!/bin/sh\nexit 1\n");
        let asset = format!("{BINARY}-{}.tar.gz", asset_suffix().expect("本机平台"));
        let fake = Fake::start(
            tag,
            vec![
                (asset.clone(), archive.clone()),
                (SUMS.to_string(), sums_for(&asset, &archive).into_bytes()),
            ],
        )
        .await;

        let dir = tempfile::tempdir().expect("临时目录");
        let dest = dir.path().join(BINARY);
        std::fs::write(&dest, "旧的").expect("铺一个现在装着的");
        let home = tempfile::tempdir().expect("临时 home");

        let error = update_against(home.path(), &client().expect("客户端"), &fake.api(), &dest)
            .await
            .expect_err("跑不起来就不该换");

        assert!(error.contains("跑不起来"), "{error}");
        assert!(unchanged(&dest, "旧的"));
        assert!(
            dotfiles(dir.path()).is_empty(),
            "{:?}",
            dotfiles(dir.path())
        );
    }

    /// 包里的那份能跑，但报的版本不是这一版（发布流程把标签和 Cargo.toml 的版本搞错了）
    /// 时同样不换——这时候装上去的会是一个"版本号对不上"的 komo。
    #[tokio::test]
    async fn a_binary_that_reports_another_version_is_not_swapped_in() {
        let tag = "v99.0.0";
        let archive = release_archive(&fake_binary("v1.0.0"));
        let asset = format!("{BINARY}-{}.tar.gz", asset_suffix().expect("本机平台"));
        let fake = Fake::start(
            tag,
            vec![
                (asset.clone(), archive.clone()),
                (SUMS.to_string(), sums_for(&asset, &archive).into_bytes()),
            ],
        )
        .await;

        let dir = tempfile::tempdir().expect("临时目录");
        let dest = dir.path().join(BINARY);
        std::fs::write(&dest, "旧的").expect("铺一个现在装着的");
        let home = tempfile::tempdir().expect("临时 home");

        let error = update_against(home.path(), &client().expect("客户端"), &fake.api(), &dest)
            .await
            .expect_err("版本对不上就不该换");

        assert!(error.contains("对不上"), "{error}");
        assert!(unchanged(&dest, "旧的"));
        assert!(
            dotfiles(dir.path()).is_empty(),
            "{:?}",
            dotfiles(dir.path())
        );
    }

    /// 这一版没有本机平台的包时，说清楚它有什么，而不是 404。
    ///
    /// 那一份"别人的包"必须是**我们从来不出的平台**：写 `linux-amd64` 之类，在 linux
    /// 机器上跑测试时它正好就是本机要的那一个，于是这条测试在 CI 上测的完全是另一件事。
    #[tokio::test]
    async fn a_release_without_our_asset_names_what_it_has() {
        let tag = "v99.0.0";
        let other = "komo-plan9-mips.tar.gz";
        let fake = Fake::start(
            tag,
            vec![
                (other.to_string(), b"x".to_vec()),
                (SUMS.to_string(), sums_for(other, b"x").into_bytes()),
            ],
        )
        .await;
        let dir = tempfile::tempdir().expect("临时目录");
        let dest = dir.path().join(BINARY);
        let home = tempfile::tempdir().expect("临时 home");

        let error = update_against(home.path(), &client().expect("客户端"), &fake.api(), &dest)
            .await
            .expect_err("没有本机平台的包");

        assert!(error.contains(other), "要说清楚它有什么：{error}");
        assert!(
            error.contains(&format!("{BINARY}-{}", asset_suffix().expect("本机平台"))),
            "要点名缺的是哪一个：{error}"
        );
        assert!(!dest.exists(), "没换上去就不该留下 dest");
    }

    /// 装着的比发布的新（开发机上跑源码构建的那份，或者回滚之后）：报告，不动。
    #[tokio::test]
    async fn an_older_release_does_not_downgrade() {
        let fake = Fake::start("v0.0.1", Vec::new()).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let dest = dir.path().join(BINARY);
        std::fs::write(&dest, "旧的").expect("铺一个现在装着的");
        let home = tempfile::tempdir().expect("临时 home");

        let printed = update_against(home.path(), &client().expect("客户端"), &fake.api(), &dest)
            .await
            .expect("比现在旧也是一条正常结论");

        assert!(printed.contains("比它旧"), "{printed}");
        assert!(unchanged(&dest, "旧的"));
    }

    /// 版本号是数字比出来的，不是比字符串：`0.10.0` 比 `0.9.9` 新。
    #[test]
    fn versions_compare_as_numbers() {
        assert!(version("v0.10.0") > version("v0.9.9"));
        assert!(version("0.8.0") == version("v0.8.0"));
        assert_eq!(version("v1.2.3"), Some((1, 2, 3)));
        // 认不出的标签宁可停下来说清楚，也不按字符串猜：
        assert_eq!(version("v1.2"), None);
        assert_eq!(version("v1.2.3.4"), None);
        assert_eq!(version("nightly"), None);
    }

    /// 校验和按文件名对行——`SHA256SUMS` 里一行一个资产。
    #[test]
    fn checksum_lines_are_matched_by_name() {
        let sums = "aa  one.tar.gz\nbb  two.tar.gz\ncc  three.tar.gz\n";
        assert_eq!(sum_for(sums, "two.tar.gz"), Some("bb".to_string()));
        assert_eq!(sum_for(sums, "three.tar.gz"), Some("cc".to_string()));
        assert_eq!(sum_for(sums, "missing.tar.gz"), None);
    }

    /// 已经是最新时只报告，什么都不下。
    #[tokio::test]
    async fn the_installed_version_is_reported_as_latest() {
        let fake = Fake::start(&format!("v{}", env!("CARGO_PKG_VERSION")), Vec::new()).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let dest = dir.path().join(BINARY);
        std::fs::write(&dest, "旧的").expect("铺一个现在装着的");
        let home = tempfile::tempdir().expect("临时 home");

        let printed = update_against(home.path(), &client().expect("客户端"), &fake.api(), &dest)
            .await
            .expect("已经是最新");

        assert!(printed.contains("已经是最新"), "{printed}");
        assert!(unchanged(&dest, "旧的"));
    }
}
