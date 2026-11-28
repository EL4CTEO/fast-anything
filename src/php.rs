//! PHP backend: apps run as  php app.php. Profiled by a one-line timer put into every function
//! for one run, read with PHP's own tokenizer, and bridged with PHP's FFI extension.

use crate::backend::{self, Profile, Recording};
use crate::hotspot::{Hotspot, Param};
use crate::native::{self, Flavor, Patches};
use crate::python::DISABLE_ENV;
use crate::state::{Ctx, Port, Verify};
use crate::util::{self, combined, output, tail};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const DESCRIBE: &str = include_str!("php/describe.php");
const PROF: &str = include_str!("php/prof.php");
const REC: &str = include_str!("php/rec.php");
const BRIDGE: &str = include_str!("php/bridge.php");
/// php options that take the next argument as their value.
const VALUE_OPTS: &[&str] = &["-d", "-c", "-z"];

pub fn is_php(program: &str) -> bool {
    let stem = util::program_stem(program);
    stem == "php" || (stem.starts_with("php") && stem[3..].chars().all(|c| c.is_ascii_digit() || c == '.'))
}

pub fn resolve(program: &str, root: &Path) -> Result<String> {
    let prog = if program.contains(['/', '\\']) { root.join(program) } else { PathBuf::from(program) };
    let out =
        output(Command::new(&prog).args(["-r", "echo PHP_BINARY, PHP_EOL, PHP_VERSION;"]).current_dir(root), "php")?;
    ensure!(out.status.success(), "{program} failed:\n{}", tail(&combined(&out), 5));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = text.lines();
    let exe = lines.next().unwrap_or_default().trim().to_string();
    let version: Vec<u32> = lines.next().unwrap_or_default().split('.').filter_map(|s| s.parse().ok()).collect();
    ensure!(version.as_slice() >= &[8, 1][..], "PHP 8.1 or newer is needed (found {version:?})");
    Ok(exe)
}

/// The script in  php [options] app.php [args].
fn entry(args: &[String]) -> Result<&str> {
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') {
        if args[i] == "-f" {
            return args.get(i + 1).map(String::as_str).context("-f needs a file");
        }
        i += if VALUE_OPTS.contains(&args[i].as_str()) { 2 } else { 1 };
    }
    args.get(i).map(String::as_str).filter(|f| f.ends_with(".php")).context("run the app as  php app.php")
}

fn source_files(root: &Path, entry: &str) -> Vec<PathBuf> {
    let start = root.join(entry).parent().map(Path::to_path_buf).unwrap_or_else(|| root.to_path_buf());
    let mut out = Vec::new();
    let mut stack = vec![start];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.is_dir() {
                if !name.starts_with('.') && !["vendor", "node_modules", "cache", "var"].contains(&name.as_str()) {
                    stack.push(p);
                }
            } else if name.ends_with(".php") {
                out.push(p);
            }
        }
    }
    out
}

#[derive(Deserialize, Clone)]
struct Func {
    path: PathBuf,
    name: String,
    class: Option<String>,
    qualname: String,
    line: u32,
    end_line: u32,
    body_offset: usize,
    params: Vec<Param>,
    returns: Option<String>,
    blockers: Vec<String>,
    warnings: Vec<String>,
    calls: Vec<String>,
    source: String,
}

fn describe(ctx: &Ctx, php: &str, files: &[PathBuf]) -> Result<Vec<Func>> {
    let script = ctx.fast.join("describe.php");
    fs::write(&script, DESCRIBE)?;
    let list = ctx.fast.join("php-files.txt");
    fs::write(&list, files.iter().map(|f| f.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n"))?;
    let out = ctx.fast.join("php-funcs.json");
    let run =
        output(Command::new(php).arg(&script).arg(&list).arg(&out).current_dir(&ctx.root).stdin(Stdio::null()), "php")?;
    ensure!(run.status.success(), "reading the PHP sources failed:\n{}", tail(&combined(&run), 20));
    serde_json::from_str(&fs::read_to_string(&out)?).context("reading the function list")
}

fn rel(ctx: &Ctx, p: &Path) -> String {
    util::forward_slashes(p.strip_prefix(&ctx.root).unwrap_or(p))
}

fn hotspot(ctx: &Ctx, f: &Func, calls: u64, own: f64, cum: f64) -> Hotspot {
    let file = rel(ctx, &f.path);
    Hotspot {
        module: Some(file.trim_end_matches(".php").replace('/', ".")),
        file,
        line: f.line,
        end_line: f.end_line,
        name: f.name.clone(),
        qualname: f.qualname.clone(),
        kind: "function".into(),
        ncalls: calls,
        tottime: own,
        cumtime: cum,
        portability: if f.blockers.is_empty() { 0.9 } else { 0.0 },
        blockers: f.blockers.clone(),
        warnings: f.warnings.clone(),
        params: f.params.clone(),
        returns: f.returns.clone(),
        source: f.source.clone(),
        calls: f.calls.clone(),
        owner: f.class.clone(),
        ..Hotspot::default()
    }
}

/// Insert text right after the { of each function (same line: no line numbers move).
/// Offsets are byte indexes into the original file (PHP's tokenizer is byte-based).
fn insert_at(text: &[u8], mut at: Vec<(usize, String)>) -> Vec<u8> {
    at.sort_by_key(|a| std::cmp::Reverse(a.0));
    let mut out = text.to_vec();
    for (offset, piece) in at {
        out.splice(offset..offset, piece.into_bytes());
    }
    out
}

fn run_php(
    ctx: &Ctx,
    php: &str,
    prepend: &Path,
    args: &[String],
    env: &[(&str, &Path)],
) -> Result<std::process::Output> {
    Ok(run_php_for(ctx, php, prepend, args, env, None)?.0)
}

/// run_php, ending the app after `duration` seconds (and stopping it from outside 15s later).
fn run_php_for(
    ctx: &Ctx,
    php: &str,
    prepend: &Path,
    args: &[String],
    env: &[(&str, &Path)],
    duration: Option<f64>,
) -> Result<(std::process::Output, bool)> {
    let mut cmd = Command::new(php);
    if let Some(d) = duration {
        cmd.env("FAST_ANYTHING_STOP_AFTER_MS", ((d * 1000.0) as u64).to_string());
    }
    cmd.arg("-d")
        // quoted, as an ini value: a path can hold characters ini syntax reads as operators (~ in
        // Windows short names, like C:\Users\RUNNER~1)
        .arg(format!("auto_prepend_file=\"{}\"", prepend.display()))
        .args(args)
        .current_dir(&ctx.root)
        .env(DISABLE_ENV, "1")
        .stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    util::output_for(
        &mut cmd,
        "php",
        duration.map(|d| Duration::from_secs_f64(d.ceil() + 15.0)),
        Duration::from_secs(10),
    )
}

/// `duration`: end the app after that many seconds (servers that run until closed).
pub fn profile(ctx: &Ctx, php: &str, args: &[String], duration: Option<f64>) -> Result<Profile> {
    let funcs = describe(ctx, php, &source_files(&ctx.root, entry(args)?))?;
    let mut by_file: BTreeMap<&Path, Vec<(usize, String)>> = BTreeMap::new();
    for (i, f) in funcs.iter().enumerate() {
        if !f.blockers.iter().any(|b| b.contains("generator")) {
            by_file
                .entry(&f.path)
                .or_default()
                .push((f.body_offset, format!(" $__faP = new \\FastAnythingProf({i});")));
        }
    }
    let prof = ctx.fast.join("prof.php");
    fs::write(&prof, PROF)?;
    let out = ctx.fast.join("profile.json");
    let _ = fs::remove_file(&out);
    let mut patches = Patches::new(ctx)?;
    for (path, at) in by_file {
        let text = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        patches.write_bytes(path, &insert_at(&text, at))?;
    }
    let run = run_php_for(ctx, php, &prof, args, &[("FAST_ANYTHING_PROF_OUT", &out)], duration);
    patches.restore()?;
    let (run, killed) = run?;
    ensure!(!killed, "the app was still running 15s after --duration: PHP needs the pcntl extension for it");
    ensure!(run.status.success(), "the app failed ({}):\n{}", run.status, tail(&combined(&run), 30));
    let result: Value = serde_json::from_str(&fs::read_to_string(&out).context("the profiler wrote nothing")?)?;
    let wall = result["wall"].as_f64().unwrap_or(0.0);
    let stats = result["stats"].as_object().cloned().unwrap_or_default();
    let stat = |i: usize| stats.get(&i.to_string()).and_then(Value::as_array).cloned();
    let mut functions = Vec::new();
    for (i, f) in funcs.iter().enumerate() {
        let helper =
            funcs.iter().enumerate().any(|(j, g)| stat(j).is_some() && g.path == f.path && g.calls.contains(&f.name));
        match stat(i) {
            Some(s) => functions.push(hotspot(
                ctx,
                f,
                s[0].as_u64().unwrap_or(0),
                s[1].as_f64().unwrap_or(0.0),
                s[2].as_f64().unwrap_or(0.0),
            )),
            None if helper => functions.push(hotspot(ctx, f, 0, 0.0, 0.0)),
            None => {}
        }
    }
    Ok(Profile { wall, base: ".".into(), functions, notes: Vec::new(), output: combined(&run) })
}

fn name_const(hs: &Hotspot) -> &'static str {
    if hs.owner.is_some() { "__METHOD__" } else { "__FUNCTION__" }
}

pub fn record(ctx: &Ctx, php: &str, args: &[String], hs: &Hotspot, out: &Path, limit: usize) -> Result<Recording> {
    let file = ctx.root.join(&hs.file);
    let funcs = describe(ctx, php, std::slice::from_ref(&file))?;
    let f = funcs
        .iter()
        .filter(|f| f.qualname == hs.qualname)
        .min_by_key(|f| (f.line as i64 - hs.line as i64).abs())
        .with_context(|| format!("{} isn't in {} any more: scan again", hs.qualname, hs.file))?;
    let call = if hs.owner.is_some() { format!("self::{}", hs.name) } else { hs.name.clone() };
    let line = format!(
        " if (!\\FastAnythingRec::$inside) return \\FastAnythingRec::record({}, func_get_args(), static fn(...$__faA) => {call}(...$__faA));",
        name_const(hs)
    );
    let text = fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
    let rec = ctx.fast.join("rec.php");
    fs::write(&rec, REC)?;
    let meta = out.with_extension("meta.json");
    let _ = fs::remove_file(&meta);
    let mut patches = Patches::new(ctx)?;
    patches.write_bytes(&file, &insert_at(&text, vec![(f.body_offset, line)]))?;
    let run = run_php(ctx, php, &rec, args, &[("FAST_ANYTHING_RECORD_OUT", out), ("FAST_ANYTHING_RECORD_META", &meta)]);
    patches.restore()?;
    let run = run?;
    ensure!(run.status.success(), "the app failed while recording ({}):\n{}", run.status, tail(&combined(&run), 30));
    let mut rec: Recording = serde_json::from_str(&fs::read_to_string(&meta).context("the recorder wrote nothing")?)?;
    rec.kept = rec.kept.min(limit.max(rec.kept));
    Ok(rec)
}

// ------------------------------------------------------------------ the crate

/// Rust type for a PHP type declaration (arrays stay serde_json::Value: their shape is in the recordings).
pub fn rust_type(t: &str) -> Option<String> {
    let t = t.trim().to_ascii_lowercase();
    if let Some(inner) = t.strip_prefix('?') {
        return Some(format!("Option<{}>", rust_type(inner)?));
    }
    Some(match t.as_str() {
        "int" => "i64".into(),
        "float" => "f64".into(),
        "string" => "String".into(),
        "bool" => "bool".into(),
        _ => return None,
    })
}

/// A PHP default value as JSON, if it's a simple literal.
pub fn default_json(value: &str) -> Option<String> {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    Some(match lower.as_str() {
        "null" | "true" | "false" => lower,
        "[]" | "array()" => "[]".into(),
        _ if v.replace('_', "").parse::<i64>().is_ok() => v.replace('_', ""),
        _ if v.parse::<f64>().is_ok() => v.into(),
        _ if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\''))) =>
        {
            serde_json::to_string(&v[1..v.len() - 1]).ok()?
        }
        _ => return None,
    })
}

pub const FLAVOR: Flavor = Flavor {
    lang: backend::Lang::Php,
    fence: "php",
    rust_type,
    default_json,
    rules: &[
        "PHP strings are bytes: `strlen`, `substr` and `$s[$i]` work on bytes, `mb_*` functions on characters. Match what the original uses.",
        "Lists arrive as JSON arrays, string-keyed arrays as JSON objects (keys in order). An empty array is `[]` either way.",
        "PHP ints are `i64` and turn into floats on overflow; `/` gives a float unless the division is exact, `intdiv` truncates.",
        "Untyped parameters arrive as `serde_json::Value`: pick concrete types from the recorded calls.",
    ],
    floats: Some(native::FLOATS),
};

pub fn scaffold(ctx: &Ctx, hs: &Hotspot, helpers: &[Hotspot], port: &Port) -> Result<String> {
    native::scaffold(ctx, hs, helpers, port, &FLAVOR)
}

pub fn verify(ctx: &Ctx, port: &Port) -> Result<Verify> {
    native::verify(ctx, port, true)
}

// ------------------------------------------------------------------ the bridge

fn bridge_class(port: &Port) -> String {
    format!("FastAnything_{}", util::slug(&port.id))
}

fn find_body(name: &str, hint: u32) -> impl Fn(&str) -> Option<usize> {
    let header = format!("function {name}(");
    let spaced = format!("function {name} (");
    move |text: &str| native::body_line(text, hint, |l| l.contains(&header) || l.contains(&spaced))
}

pub fn enable(ctx: &Ctx, port: &Port) -> Result<()> {
    let hs = ctx
        .state
        .hotspots
        .iter()
        .find(|h| h.id == port.id)
        .with_context(|| format!("{} isn't in the last scan", port.id))?;
    let file = ctx.root.join(&port.file);
    let text = fs::read_to_string(&file).with_context(|| format!("reading {}", port.file))?;
    ensure!(
        !text.trim_end().ends_with("?>"),
        "{} ends with ?>: remove the closing tag so code can follow it",
        port.file
    );
    // with braced namespaces, no code may follow them outside a namespace block: the bridge goes in
    // a global one of its own, and the function names it from the root
    let braced = text.lines().any(|l| l.trim_start().starts_with("namespace ") && l.trim_end().ends_with('{'));
    let class = bridge_class(port);
    let from = file.parent().context("file has no folder")?;
    let (_, lib) = port.lang.artifact(&port.ext);
    let lib_rel = util::forward_slashes(&util::relative(from, &ctx.lib_dir().join(lib)));
    let lib_lit = format!("'{}'", lib_rel.replace('\\', "\\\\").replace('\'', "\\'"));
    let bridge = BRIDGE.replace("/*CLASS*/", &class).replace("/*ID*/", &port.id).replace("/*LIB*/", &lib_lit);
    // in the file's own line endings (a Windows checkout gives bridge.php CRLF), so disable finds
    // what it added
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let bridge = bridge.replace("\r\n", "\n").replace('\n', nl);
    let (bridge, class) =
        if braced { (format!("namespace {{{nl}{bridge}{nl}}}"), format!("\\{class}")) } else { (bridge, class) };
    let prologue =
        format!("if ({class}::$on ?? {class}::load()) return {class}::call({}, func_get_args());", name_const(hs));
    let patched =
        native::insert_in_body(&text, "//", &port.id, &prologue, find_body(&port.func, port.line.max(hs.line)))?;
    let patched = backend::append_marked(&patched, "//", &format!("{} bridge", port.id), &bridge);
    fs::write(&file, patched).with_context(|| format!("writing {}", port.file))
}

pub fn disable(ctx: &Ctx, port: &Port) -> Result<()> {
    let file = ctx.root.join(&port.file);
    let text = fs::read_to_string(&file).with_context(|| format!("reading {}", port.file))?;
    let text = native::strip_in_body(&text, "//", &port.id);
    let text = backend::strip_marked(&text, "//", &format!("{} bridge", port.id));
    fs::write(&file, text).with_context(|| format!("writing {}", port.file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_functions_in_braced_namespaces() {
        if !Command::new("php").arg("-v").output().is_ok_and(|o| o.status.success()) {
            eprintln!("no php on PATH, skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("fa-php-braced-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let app = "<?php\nnamespace Text {\n    function width(string $s): int\n    {\n        return strlen($s);\n    }\n}\n\nnamespace {\n    echo \\Text\\width('h\u{e9}llo'), \"\\n\";\n}\n";
        fs::write(dir.join("app.php"), app).unwrap();
        let mut ctx = Ctx::open(&dir).unwrap();
        let found = profile(&ctx, "php", &["app.php".into()], None).unwrap();
        assert!(found.functions.iter().any(|f| f.name == "width" && f.blockers.is_empty()), "{:?}", found.functions);
        let hs = Hotspot {
            id: "Text.width".into(),
            file: "app.php".into(),
            line: 3,
            name: "width".into(),
            ..Hotspot::default()
        };
        ctx.state.hotspots.push(hs);
        let port = Port {
            id: "Text.width".into(),
            file: "app.php".into(),
            module: "Text".into(),
            func: "width".into(),
            ext: "fa_text_width".into(),
            crate_dir: ".fast/ports/Text.width".into(),
            recorded: 1,
            calls_seen: 1,
            verify: None,
            enabled: false,
            lang: backend::Lang::Php,
            owner: None,
            line: 3,
            sources: vec![],
        };
        enable(&ctx, &port).unwrap();
        // without the port's library, the bridge doesn't load and the PHP version runs
        let lint = Command::new("php").args(["-l", "app.php"]).current_dir(&dir).output().unwrap();
        let run = Command::new("php").arg("app.php").current_dir(&dir).output().unwrap();
        let enabled = fs::read_to_string(dir.join("app.php")).unwrap();
        disable(&ctx, &port).unwrap();
        let after = fs::read_to_string(dir.join("app.php")).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(lint.status.success(), "{}{enabled}", String::from_utf8_lossy(&lint.stdout));
        assert_eq!(String::from_utf8_lossy(&run.stdout), "6\n", "{}", String::from_utf8_lossy(&run.stderr));
        assert!(enabled.contains("namespace {\nfinal class"), "{enabled}");
        assert_eq!(after, app);
    }

    #[test]
    fn duration_ends_an_app_that_runs_until_closed() {
        let pcntl = Command::new("php").args(["-r", "echo function_exists('pcntl_alarm') ? 1 : 0;"]).output();
        if !pcntl.is_ok_and(|o| o.status.success() && o.stdout == b"1") {
            eprintln!("no php with pcntl on PATH, skipping");
            return;
        }
        let tmp = std::env::temp_dir();
        // macOS's temp folder is behind a symlink; the profile has the real paths
        let tmp = if cfg!(windows) { tmp } else { tmp.canonicalize().unwrap() };
        let dir = tmp.join(format!("fa-php-duration-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // a server's shape: some work, then a long blocking wait
        let app = "<?php\nfunction work(int $n): int\n{\n    $s = 0;\n    for ($i = 0; $i < $n; $i++) $s += $i * $i % 7;\n    return $s;\n}\n\nfor ($k = 0; $k < 3; $k++) work(300000);\nsleep(3600);\n";
        fs::write(dir.join("app.php"), app).unwrap();
        let ctx = Ctx::open(&dir).unwrap();
        let p = profile(&ctx, "php", &["app.php".into()], Some(1.0));
        let after = fs::read_to_string(dir.join("app.php")).unwrap();
        let _ = fs::remove_dir_all(&dir);
        let p = p.unwrap();
        assert!(p.functions.iter().any(|f| f.name == "work" && f.ncalls == 3), "{:?}", p.functions);
        assert!((0.9..10.0).contains(&p.wall), "{}", p.wall);
        assert_eq!(after, app);
    }

    #[test]
    fn nan_and_infinity_travel_tagged() {
        if !Command::new("php").arg("-v").output().is_ok_and(|o| o.status.success()) {
            eprintln!("no php on PATH, skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("fa-php-floats-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("rec.php"), REC).unwrap();
        let bridge = BRIDGE.replace("/*CLASS*/", "Bridge").replace("/*ID*/", "scale").replace("/*LIB*/", "'none'");
        fs::write(dir.join("bridge.php"), format!("<?php\n{bridge}\n")).unwrap();
        // the recorder, then the bridge's own conversions both ways (the port isn't built here)
        let check = r#"<?php
            require __DIR__ . '/rec.php';
            require __DIR__ . '/bridge.php';
            function scale(array $xs, float $k): array
            {
                return array_map(fn($x) => $x * $k, $xs);
            }
            FastAnythingRec::record('scale', [[1.5, INF], -INF], 'scale');
            FastAnythingRec::record('scale', [[0.0], 2.0], 'scale');
            $wire = new ReflectionMethod('Bridge', 'wire');
            $unwire = new ReflectionMethod('Bridge', 'unwire');
            $sent = ['xs' => [NAN, -0.5, -INF], 'm' => ['k' => INF]];
            echo json_encode($wire->invoke(null, $sent)), "\n";
            $back = $unwire->invoke(null, json_decode('{"a":[1.5,{"$float":"NaN"},{"$float":"Infinity"},{"$float":"-Infinity"}],"b":{"$float":"x"},"c":[]}', true));
            var_export([$back['a'][0], is_nan($back['a'][1]), $back['a'][2], $back['a'][3], $back['b'], $back['c']]);
        "#;
        fs::write(dir.join("check.php"), check).unwrap();
        let out = dir.join("rec.jsonl");
        let run = Command::new("php")
            .arg("check.php")
            .current_dir(&dir)
            .env("FAST_ANYTHING_RECORD_OUT", &out)
            .env("FAST_ANYTHING_RECORD_META", dir.join("meta.json"))
            .output()
            .unwrap();
        let text = fs::read_to_string(&out).unwrap_or_default();
        let _ = fs::remove_dir_all(&dir);
        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(run.status.success(), "{stdout}{}", String::from_utf8_lossy(&run.stderr));
        let recorded: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|c| c.get("derived").is_none())
            .collect();
        assert_eq!(
            recorded[0],
            serde_json::json!({"args": [[1.5, {"$float": "Infinity"}], {"$float": "-Infinity"}],
                               "result": [{"$float": "-Infinity"}, {"$float": "-Infinity"}]}),
            "{text}"
        );
        // and NaN and infinity are edge cases now that they can be passed
        let edge = |want: &str| {
            text.lines()
                .map(|l| serde_json::from_str::<Value>(l).unwrap())
                .any(|c| c["derived"] == "edge" && c["args"][1] == serde_json::json!({"$float": want}))
        };
        assert!(edge("NaN") && edge("Infinity") && edge("-Infinity"), "{text}");
        let mut lines = stdout.lines();
        assert_eq!(
            lines.next(),
            Some(r#"{"xs":[{"$float":"NaN"},-0.5,{"$float":"-Infinity"}],"m":{"k":{"$float":"Infinity"}}}"#)
        );
        let rest: String = lines.collect::<Vec<_>>().join("\n");
        assert!(
            rest.contains("1.5,\n  1 => true,\n  2 => INF,\n  3 => -INF,\n  4 => \n  array (\n    '$float' => 'x',"),
            "{rest}"
        );
    }

    #[test]
    fn the_recorder_adds_edge_cases() {
        use serde_json::json;
        if !Command::new("php").arg("-v").output().is_ok_and(|o| o.status.success()) {
            eprintln!("no php on PATH, skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("fa-php-edges-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("rec.php"), REC).unwrap();
        // width throws on an emoji: inputs it throws on aren't fair tests and must be left out
        let check = r#"<?php
            require __DIR__ . '/rec.php';
            function width(string $s, array $xs): int
            {
                if (str_contains($s, "\u{1f44d}")) throw new InvalidArgumentException('emoji');
                return preg_match_all('/./su', $s) + count($xs);
            }
            $s = str_repeat('word ', 40);
            for ($k = 0; $k < 3; $k++) {
                FastAnythingRec::record('width', [$s, array_fill(0, 20 + $k, 0)], 'width');
            }
        "#;
        fs::write(dir.join("check.php"), check).unwrap();
        let out = dir.join("rec.jsonl");
        let run = Command::new("php")
            .arg("check.php")
            .current_dir(&dir)
            .env("FAST_ANYTHING_RECORD_OUT", &out)
            .env("FAST_ANYTHING_RECORD_META", dir.join("meta.json"))
            .output()
            .unwrap();
        let text = fs::read_to_string(&out).unwrap_or_default();
        let _ = fs::remove_dir_all(&dir);
        assert!(
            run.status.success(),
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        let edges: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|c| c["derived"] == "edge")
            .collect();
        let has_arg = |want: &Value, i: usize| edges.iter().any(|c| &c["args"][i] == want);
        assert!(has_arg(&json!(""), 0) && has_arg(&json!([]), 1), "empty inputs: {text}");
        // non-ASCII text, added to the short version of the recorded input (64 characters, 8 items)
        let short: String = "word ".repeat(40).chars().take(64).collect();
        assert!(has_arg(&json!(format!("{short}\u{65e5}\u{672c}")), 0), "{text}");
        assert!(!text.contains('\u{1f44d}'), "an input the original throws on");
        assert!(edges.iter().any(|c| c["args"][1].as_array().is_some_and(|a| a.len() == 8 * 16)), "bigger arrays");
        assert!(edges.iter().any(|c| c["args"][1][0] == json!(-1)), "special numbers in arrays");
        assert!(edges.iter().all(|c| c["result"].is_i64()), "every edge has the original's answer");
        assert!(edges.len() <= 100);
    }

    #[test]
    fn php_bits() {
        assert!(is_php("php") && is_php("php8.3") && is_php("C:\\php\\php.exe") && !is_php("phpunit"));
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(entry(&s(&["-d", "memory_limit=1G", "app.php", "x"])).unwrap(), "app.php");
        assert_eq!(default_json("0.85").as_deref(), Some("0.85"));
        assert_eq!(default_json("NULL").as_deref(), Some("null"));
        assert_eq!(default_json("array()").as_deref(), Some("[]"));
        assert_eq!(rust_type("?int").as_deref(), Some("Option<i64>"));
        assert_eq!(insert_at(b"function f() {\n}", vec![(14, " X;".into())]), b"function f() { X;\n}");
        // Latin-1 0xe9 is not UTF-8; insert still works at a byte offset
        let latin1 = b"function f() {\n // caf\xe9\n}";
        let patched = insert_at(latin1, vec![(14, " X;".into())]);
        assert_eq!(&patched[..17], b"function f() { X;");
        assert!(patched.contains(&0xe9), "{patched:?}");
    }

    #[test]
    fn describes_trailing_comma_and_empty_params() {
        if !Command::new("php").arg("-v").output().is_ok_and(|o| o.status.success()) {
            eprintln!("no php on PATH, skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("fa-php-params-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let app = "<?php\nfunction add(\n    int $a,\n    int $b,\n): int\n{\n    return $a + $b;\n}\nfinal class Greeter\n{\n    public function __construct(\n    ) {\n    }\n    public static function hi(string $n): string\n    {\n        return \"hi $n\";\n    }\n}\n$t = 0; for ($i = 0; $i < 1000; $i++) $t += add($i, 1);\necho Greeter::hi('x'), $t, \"\\n\";\n";
        fs::write(dir.join("app.php"), app).unwrap();
        let ctx = Ctx::open(&dir).unwrap();
        let found = describe(&ctx, "php", &[dir.join("app.php")]).unwrap();
        let _ = fs::remove_dir_all(&dir);
        let add = found.iter().find(|f| f.name == "add").unwrap();
        assert_eq!(add.params.len(), 2, "{:?}", add.params);
        assert_eq!(add.params[0].name, "a");
        assert_eq!(add.params[1].name, "b");
        let ctor = found.iter().find(|f| f.name == "__construct").unwrap();
        assert!(ctor.params.is_empty(), "{:?}", ctor.params);
        let hi = found.iter().find(|f| f.name == "hi").unwrap();
        assert_eq!(hi.params.len(), 1);
        assert_eq!(hi.params[0].name, "n");
    }

    #[test]
    fn profiles_latin1_source_comments() {
        if !Command::new("php").arg("-v").output().is_ok_and(|o| o.status.success()) {
            eprintln!("no php on PATH, skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("fa-php-latin1-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("app.php"),
            b"<?php\nrequire __DIR__.'/lib.php';\n$t = 0; for ($i = 0; $i < 100000; $i++) $t += twice($i);\necho $t, \"\\n\";\n",
        )
        .unwrap();
        // Latin-1 0xe9 in a comment — PHP runs fine; scan must not abort
        fs::write(
            dir.join("lib.php"),
            b"<?php\n// caf\xe9 (Latin-1 byte in a comment)\nfunction twice(int $x): int\n{\n    return $x * 2;\n}\n",
        )
        .unwrap();
        let ctx = Ctx::open(&dir).unwrap();
        let p = profile(&ctx, "php", &["app.php".into()], None);
        let after_app = fs::read(dir.join("app.php")).unwrap();
        let after_lib = fs::read(dir.join("lib.php")).unwrap();
        let _ = fs::remove_dir_all(&dir);
        let p = p.expect("scan should tolerate Latin-1 comments");
        assert!(p.functions.iter().any(|f| f.name == "twice" && f.ncalls == 100000), "{:?}", p.functions);
        assert_eq!(after_lib.last(), Some(&b'\n'));
        assert!(after_lib.contains(&0xe9), "Latin-1 byte restored");
        assert!(after_app.starts_with(b"<?php"));
    }
}
