//! STEP / IGES / BREP reader: runs OpenCascade (occt-import-js, compiled to WebAssembly) in
//! wasmtime.
//!
//! The kernel is `occt.wasm` next to the DLL, prepared by `scripts/build-occt.mjs` (real
//! Emscripten import/export names, standard exnref exceptions). The library's API is embind:
//! it builds its result with `emscripten::val` calls into JavaScript. [`Host`] implements the
//! small subset of that glue it uses (objects, arrays, numbers, strings) plus the Emscripten
//! system imports, so no JavaScript engine is needed.
//!
//! Compiling the module takes a few seconds, so the native code is cached in
//! [`crate::cache_dir`] (warmed by [`warm_up`] at registration). Each file gets a fresh instance,
//! runs on its own thread with a large stack, and is stopped after [`TIMEOUT`].

use std::{
    cell::RefCell,
    collections::HashMap,
    hash::{Hash, Hasher},
    path::Path,
    rc::Rc,
    sync::OnceLock,
    time::Duration,
};

use wasmtime::{
    bail, ensure, format_err, Caller, Config, Engine, InstancePre, Linker, Memory, Module, Store,
    StoreLimits, StoreLimitsBuilder, TypedFunc, Val,
};

use super::{srgb_to_linear, Mesh, Model, DEFAULT_COLOR};

/// Larger files are skipped: the input is copied several times on its way into the kernel.
const MAX_INPUT: usize = 256 << 20;
/// Give up on files that take longer than this to tessellate.
const TIMEOUT: Duration = Duration::from_secs(30);
const TICK: Duration = Duration::from_millis(250);
/// Upper bound for the kernel's heap (the wasm32 limit is 4 GiB).
const MAX_HEAP: usize = 2 << 30;
/// Native stack for the conversion thread; OCCT recurses deeply on some files.
const THREAD_STACK: usize = 16 << 20;
/// wasmtime caps this below its (unused) async stack size of 2 MiB; the default is 512 KiB.
const WASM_STACK: usize = 1 << 20;

/// Tessellation for thumbnails: coarse, relative to the model's size.
const LINEAR_DEFLECTION: f64 = 0.005;
const ANGULAR_DEFLECTION: f64 = 0.8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CadFormat {
    Step,
    Iges,
    Brep,
}

impl CadFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Step => "step",
            Self::Iges => "iges",
            Self::Brep => "brep",
        }
    }
}

pub fn parse(bytes: &[u8], format: CadFormat) -> Result<Model, String> {
    if bytes.len() > MAX_INPUT {
        return Err(format!(
            "{} MB is too large for a CAD thumbnail",
            bytes.len() >> 20
        ));
    }
    let kernel = kernel()?;
    let owned = bytes.to_vec();
    std::thread::Builder::new()
        .name("occt".into())
        .stack_size(THREAD_STACK)
        .spawn(move || kernel.convert(&owned, format))
        .map_err(|e| e.to_string())?
        .join()
        .map_err(|_| "OpenCascade thread panicked".to_string())?
}

/// Compiles the kernel (or loads it from the cache) ahead of the first thumbnail.
pub fn warm_up() -> Result<(), String> {
    kernel().map(|_| ())
}

struct Kernel {
    engine: Engine,
    pre: InstancePre<Host>,
}

fn kernel() -> Result<&'static Kernel, String> {
    static KERNEL: OnceLock<Result<Kernel, String>> = OnceLock::new();
    KERNEL
        .get_or_init(|| {
            let started = std::time::Instant::now();
            let kernel = Kernel::load();
            match &kernel {
                Ok(_) => crate::log(&format!("OpenCascade ready in {:?}", started.elapsed())),
                Err(e) => crate::log(&format!("OpenCascade unavailable: {e}")),
            }
            kernel
        })
        .as_ref()
        .map_err(Clone::clone)
}

impl Kernel {
    fn load() -> Result<Self, String> {
        let path = crate::occt_wasm_path().ok_or("OpenCascade kernel location unknown")?;
        let wasm =
            std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;

        let mut config = Config::new();
        config.wasm_exceptions(true);
        config.epoch_interruption(true);
        config.max_wasm_stack(WASM_STACK);
        let engine = Engine::new(&config).map_err(|e| e.to_string())?;
        let module = compile(&engine, &wasm)?;

        let mut linker = Linker::new(&engine);
        define_imports(&mut linker).map_err(|e| e.to_string())?;
        let pre = linker
            .instantiate_pre(&module)
            .map_err(|e| format!("cannot link OpenCascade: {e}"))?;

        // Epoch ticker for the per-conversion deadline. It lives as long as the process.
        let ticker = engine.clone();
        std::thread::Builder::new()
            .name("occt-epoch".into())
            .spawn(move || loop {
                std::thread::sleep(TICK);
                ticker.increment_epoch();
            })
            .map_err(|e| e.to_string())?;

        Ok(Self { engine, pre })
    }

    fn convert(&self, bytes: &[u8], format: CadFormat) -> Result<Model, String> {
        let mut store = Store::new(&self.engine, Host::default());
        store.limiter(|h| &mut h.limits);
        store.set_epoch_deadline((TIMEOUT.as_millis() / TICK.as_millis()) as u64);
        store.epoch_deadline_trap();

        let instance = self
            .pre
            .instantiate(&mut store)
            .map_err(|e| e.to_string())?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or("no memory export")?;
        let malloc = instance
            .get_typed_func::<u32, u32>(&mut store, "malloc")
            .map_err(|e| e.to_string())?;
        let free = instance
            .get_typed_func::<u32, ()>(&mut store, "free")
            .map_err(|e| e.to_string())?;
        let table = instance
            .get_table(&mut store, "__indirect_function_table")
            .ok_or("no function table export")?;
        let host = store.data_mut();
        host.memory = Some(memory);
        host.malloc = Some(malloc);
        host.free = Some(free.clone());

        // Runs the embind registrations (EMSCRIPTEN_BINDINGS) among the static constructors.
        instance
            .get_typed_func::<(), ()>(&mut store, "__wasm_call_ctors")
            .and_then(|f| f.call(&mut store, ()))
            .map_err(|e| format!("OpenCascade init failed: {e}"))?;

        let read_file = store
            .data()
            .functions
            .get("ReadFile")
            .cloned()
            .ok_or("ReadFile is not registered")?;
        let invoker = table
            .get(&mut store, read_file.invoker as u64)
            .and_then(|r| r.as_func().flatten().copied())
            .ok_or("ReadFile invoker not found")?;

        let format_wire = alloc_std_string(&mut store, format.name().as_bytes())?;
        let host = store.data_mut();
        let buffer = host.new_handle(Value::Bytes(Rc::from(bytes)));
        let params = host.new_handle(Value::object([
            ("linearUnit", Value::str("millimeter")),
            ("linearDeflectionType", Value::str("bounding_box_ratio")),
            ("linearDeflection", Value::Number(LINEAR_DEFLECTION)),
            ("angularDeflection", Value::Number(ANGULAR_DEFLECTION)),
        ]));

        let mut results = [Val::I32(0)];
        invoker
            .call(
                &mut store,
                &[
                    Val::I32(read_file.function as i32),
                    Val::I32(format_wire as i32),
                    Val::I32(buffer as i32),
                    Val::I32(params as i32),
                ],
                &mut results,
            )
            .map_err(|e| format!("OpenCascade failed: {e:?}"))?;
        free.call(&mut store, format_wire)
            .map_err(|e| e.to_string())?;

        let handle = results[0].unwrap_i32() as u32;
        let host = store.data_mut();
        let result = host.value(handle);
        host.decref(handle);
        to_model(&result)
    }
}

/// Loads the compiled module from the cache, or compiles and caches it.
fn compile(engine: &Engine, wasm: &[u8]) -> Result<Module, String> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut hasher);
    let file_name = format!("occt-{:016x}-{:016x}.cwasm", fnv1a(wasm), hasher.finish());
    let cache = crate::cache_dir().map(|d| d.join(file_name));

    if let Some(path) = cache.as_deref().filter(|p| p.is_file()) {
        // SAFETY: the cache is in the user's own profile (LocalAppData), which only processes
        // at the user's integrity level can write, the same as the DLL's install folder.
        // wasmtime also checks that the file was produced by this engine configuration.
        match unsafe { Module::deserialize_file(engine, path) } {
            Ok(module) => return Ok(module),
            Err(e) => crate::log(&format!("ignoring OpenCascade cache: {e}")),
        }
    }

    let module =
        Module::new(engine, wasm).map_err(|e| format!("cannot compile OpenCascade: {e}"))?;
    if let Some(path) = cache {
        if let Err(e) = store_cache(&module, &path) {
            crate::log(&format!("cannot cache OpenCascade: {e}"));
        }
    }
    Ok(module)
}

fn store_cache(module: &Module, path: &Path) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let bytes = module.serialize().map_err(std::io::Error::other)?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    // Drop entries from older kernels or DLL versions.
    for entry in std::fs::read_dir(dir)?.flatten() {
        let p = entry.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if p != path && name.starts_with("occt-") && name.ends_with(".cwasm") {
            let _ = std::fs::remove_file(p);
        }
    }
    Ok(())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    })
}

// --- result -> Model ---

fn to_model(result: &Value) -> Result<Model, String> {
    if !matches!(result.get("success"), Value::Bool(true)) {
        return Err("OpenCascade could not read the file".into());
    }
    let meshes = result.get("meshes");
    let meshes = meshes.as_array().ok_or("result has no meshes")?;
    let mut model = Model::default();
    for mesh in meshes.iter() {
        model.meshes.push(to_mesh(mesh)?);
    }
    Ok(model)
}

fn numbers(v: &Value) -> Vec<f64> {
    v.as_array()
        .map(|a| a.iter().map(Value::as_number).collect())
        .unwrap_or_default()
}

fn color(v: &Value) -> Option<[f32; 3]> {
    let c = numbers(v);
    (c.len() >= 3).then(|| [0, 1, 2].map(|i| srgb_to_linear(c[i] as f32)))
}

fn to_mesh(v: &Value) -> Result<Mesh, String> {
    let flat = numbers(&v.get("attributes").get("position").get("array"));
    // CAD is Z-up; swap into the viewer's Y-up space, like the STL reader does.
    let positions: Vec<[f32; 3]> = flat
        .chunks_exact(3)
        .map(|p| [p[0] as f32, p[2] as f32, p[1] as f32])
        .collect();
    let indices: Vec<u32> = numbers(&v.get("index").get("array"))
        .into_iter()
        .map(|i| i as u32)
        .collect();
    if indices.iter().any(|&i| i as usize >= positions.len()) {
        return Err("mesh index out of range".into());
    }
    let base_color = color(&v.get("color")).unwrap_or(DEFAULT_COLOR);

    let faces = v.get("brep_faces");
    let faces = faces.as_array();
    let mut colors: Option<Vec<[f32; 3]>> = None;
    for face in faces.iter().flat_map(|f| f.iter()) {
        let Some(c) = color(&face.get("color")) else {
            continue;
        };
        let colors = colors.get_or_insert_with(|| vec![base_color; positions.len()]);
        let first = face.get("first").as_number() as usize;
        let last = (face.get("last").as_number() as usize).min(indices.len() / 3);
        for tri in first..=last {
            for &i in indices.get(tri * 3..tri * 3 + 3).unwrap_or_default() {
                colors[i as usize] = c;
            }
        }
    }

    Ok(Mesh {
        positions,
        indices,
        colors,
        base_color,
    })
}

// --- emval: the JavaScript values embind code creates and reads ---

/// A JavaScript object's own properties, in insertion order.
type Properties = Vec<(Rc<str>, Value)>;

#[derive(Clone, Default)]
enum Value {
    #[default]
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    Str(Rc<str>),
    /// The input file (a `Uint8Array` in JavaScript).
    Bytes(Rc<[u8]>),
    Object(Rc<RefCell<Properties>>),
    Array(Rc<RefCell<Vec<Value>>>),
    /// A `typed_memory_view` into linear memory: element kind, pointer, element count.
    MemoryView(u32, u32, u32),
    /// Pointers to free when the destructor list runs.
    Destructors(Rc<[u32]>),
    /// The JavaScript built-ins embind reaches for, e.g. `Object.prototype.hasOwnProperty`.
    Builtin(&'static str),
}

impl Value {
    fn str(s: &str) -> Self {
        Self::Str(Rc::from(s))
    }

    fn object<const N: usize>(props: [(&str, Value); N]) -> Self {
        Self::Object(Rc::new(RefCell::new(
            props.into_iter().map(|(k, v)| (Rc::from(k), v)).collect(),
        )))
    }

    fn get(&self, key: &str) -> Value {
        self.property(&Value::str(key))
    }

    fn property(&self, key: &Value) -> Value {
        match (self, key) {
            (Self::Object(props), Self::Str(k)) => props
                .borrow()
                .iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default(),
            (Self::Array(items), Self::Number(i)) => {
                items.borrow().get(*i as usize).cloned().unwrap_or_default()
            }
            (Self::Array(items), Self::Str(k)) if &**k == "length" => {
                Self::Number(items.borrow().len() as f64)
            }
            (Self::Bytes(b), Self::Number(i)) => b
                .get(*i as usize)
                .map(|&x| Self::Number(x as f64))
                .unwrap_or_default(),
            (Self::Bytes(b), Self::Str(k)) if &**k == "length" => Self::Number(b.len() as f64),
            (Self::Str(s), Self::Str(k)) if &**k == "length" => Self::Number(s.len() as f64),
            (Self::Builtin("Object"), Self::Str(k)) if &**k == "prototype" => {
                Self::Builtin("Object.prototype")
            }
            (Self::Builtin("Object.prototype"), Self::Str(k)) if &**k == "hasOwnProperty" => {
                Self::Builtin("hasOwnProperty")
            }
            _ => Self::Undefined,
        }
    }

    fn set_property(&self, key: Value, value: Value) {
        match (self, key) {
            (Self::Object(props), key) => {
                let key: Rc<str> = match key {
                    Self::Str(s) => s,
                    Self::Number(n) => Rc::from(n.to_string()),
                    _ => return,
                };
                let mut props = props.borrow_mut();
                match props.iter_mut().find(|(name, _)| *name == key) {
                    Some(slot) => slot.1 = value,
                    None => props.push((key, value)),
                }
            }
            (Self::Array(items), Self::Number(i)) if i >= 0.0 => {
                let i = i as usize;
                let mut items = items.borrow_mut();
                if i >= items.len() {
                    items.resize(i + 1, Self::Undefined);
                }
                items[i] = value;
            }
            _ => {}
        }
    }

    fn as_array(&self) -> Option<std::cell::Ref<'_, Vec<Value>>> {
        match self {
            Self::Array(items) => Some(items.borrow()),
            _ => None,
        }
    }

    fn as_number(&self) -> f64 {
        match self {
            Self::Number(n) => *n,
            Self::Bool(b) => *b as u8 as f64,
            Self::Null => 0.0,
            _ => f64::NAN,
        }
    }

    fn kind(&self) -> String {
        match self {
            Self::Undefined => "undefined".into(),
            Self::Null => "null".into(),
            Self::Bool(b) => b.to_string(),
            Self::Number(n) => n.to_string(),
            Self::Str(s) => format!("{s:?}"),
            Self::Bytes(b) => format!("Uint8Array({})", b.len()),
            Self::Object(_) => "object".into(),
            Self::Array(a) => format!("Array({})", a.borrow().len()),
            Self::MemoryView(k, _, n) => format!("view{k}({n})"),
            Self::Destructors(_) => "destructors".into(),
            Self::Builtin(name) => (*name).into(),
        }
    }

    fn truthy(&self) -> bool {
        match self {
            Self::Undefined | Self::Null => false,
            Self::Bool(b) => *b,
            Self::Number(n) => *n != 0.0 && !n.is_nan(),
            Self::Str(s) => !s.is_empty(),
            _ => true,
        }
    }
}

/// How a registered C++ type crosses the boundary (embind's `registerType` entries).
#[derive(Clone, Copy)]
enum WireType {
    Void,
    Bool {
        yes: u32,
        no: u32,
    },
    Int {
        size: u32,
        signed: bool,
    },
    Float {
        size: u32,
    },
    /// `std::string` and friends: a malloc'ed `[u32 length][bytes]` block.
    String,
    WString,
    Emval,
    MemoryView {
        kind: u32,
    },
    BigInt,
}

#[derive(Clone)]
struct Function {
    /// Table index of embind's invoker trampoline.
    invoker: u32,
    /// The C++ function pointer the invoker calls.
    function: u32,
}

struct MethodCaller {
    ret: u32,
    args: Vec<u32>,
}

/// Per-instance state behind the imports.
struct Host {
    /// emval handle table. Handles 2/4/6/8 are undefined/null/true/false; others are even
    /// numbers from 10, like the JavaScript glue (C++ compares against the reserved ones).
    slots: Vec<Option<(Value, u32)>>,
    free_slots: Vec<usize>,
    types: HashMap<u32, WireType>,
    functions: HashMap<String, Function>,
    callers: Vec<MethodCaller>,
    fs: MemFs,
    memory: Option<Memory>,
    malloc: Option<TypedFunc<u32, u32>>,
    free: Option<TypedFunc<u32, ()>>,
    limits: StoreLimits,
}

impl Default for Host {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            free_slots: Vec::new(),
            types: HashMap::new(),
            functions: HashMap::new(),
            callers: Vec::new(),
            fs: MemFs::default(),
            memory: None,
            malloc: None,
            free: None,
            limits: StoreLimitsBuilder::new().memory_size(MAX_HEAP).build(),
        }
    }
}

const HANDLE_UNDEFINED: u32 = 2;
const HANDLE_NULL: u32 = 4;
const HANDLE_TRUE: u32 = 6;
const HANDLE_FALSE: u32 = 8;
const FIRST_HANDLE: u32 = 10;

impl Host {
    fn new_handle(&mut self, value: Value) -> u32 {
        match value {
            Value::Undefined => HANDLE_UNDEFINED,
            Value::Null => HANDLE_NULL,
            Value::Bool(true) => HANDLE_TRUE,
            Value::Bool(false) => HANDLE_FALSE,
            value => {
                let slot = match self.free_slots.pop() {
                    Some(i) => {
                        self.slots[i] = Some((value, 1));
                        i
                    }
                    None => {
                        self.slots.push(Some((value, 1)));
                        self.slots.len() - 1
                    }
                };
                FIRST_HANDLE + 2 * slot as u32
            }
        }
    }

    fn slot(handle: u32) -> Option<usize> {
        (handle >= FIRST_HANDLE && handle.is_multiple_of(2))
            .then(|| ((handle - FIRST_HANDLE) / 2) as usize)
    }

    fn value(&self, handle: u32) -> Value {
        match handle {
            HANDLE_NULL => Value::Null,
            HANDLE_TRUE => Value::Bool(true),
            HANDLE_FALSE => Value::Bool(false),
            h => Self::slot(h)
                .and_then(|i| self.slots.get(i))
                .and_then(|s| s.as_ref())
                .map(|(v, _)| v.clone())
                .unwrap_or_default(),
        }
    }

    fn incref(&mut self, handle: u32) {
        if let Some(Some((_, count))) = Self::slot(handle).and_then(|i| self.slots.get_mut(i)) {
            *count += 1;
        }
    }

    fn decref(&mut self, handle: u32) {
        let Some(i) = Self::slot(handle) else { return };
        if let Some(entry @ Some(_)) = self.slots.get_mut(i) {
            let (_, count) = entry.as_mut().unwrap();
            *count -= 1;
            if *count == 0 {
                *entry = None;
                self.free_slots.push(i);
            }
        }
    }

    fn wire_type(&self, raw: u32) -> wasmtime::Result<WireType> {
        self.types
            .get(&raw)
            .copied()
            .ok_or_else(|| format_err!("unregistered embind type {raw:#x}"))
    }
}

// --- linear memory helpers ---

type Ctx<'a> = Caller<'a, Host>;

fn memory(caller: &Ctx) -> wasmtime::Result<Memory> {
    caller
        .data()
        .memory
        .ok_or_else(|| format_err!("memory not ready"))
}

fn read_bytes(caller: &Ctx, ptr: u32, len: usize) -> wasmtime::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    memory(caller)?.read(caller, ptr as usize, &mut buf)?;
    Ok(buf)
}

/// Fixed-size read without allocating: embind moves the input file one byte per call.
fn read_array<const N: usize>(caller: &Ctx, ptr: u32) -> wasmtime::Result<[u8; N]> {
    let start = ptr as usize;
    memory(caller)?
        .data(caller)
        .get(start..start + N)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| format_err!("out of bounds read"))
}

fn read_u32(caller: &Ctx, ptr: u32) -> wasmtime::Result<u32> {
    Ok(u32::from_le_bytes(read_array(caller, ptr)?))
}

fn write(caller: &mut Ctx, ptr: u32, bytes: &[u8]) -> wasmtime::Result<()> {
    memory(caller)?.write(caller, ptr as usize, bytes)?;
    Ok(())
}

/// NUL-terminated Latin-1 string (embind type and symbol names).
fn read_cstr(caller: &Ctx, ptr: u32) -> wasmtime::Result<String> {
    let mem = memory(caller)?;
    let data = mem.data(caller);
    let start = ptr as usize;
    let tail = data
        .get(start..)
        .ok_or_else(|| format_err!("bad pointer"))?;
    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
    Ok(tail[..end].iter().map(|&b| b as char).collect())
}

fn malloc(caller: &mut Ctx, size: u32) -> wasmtime::Result<u32> {
    let f = caller
        .data()
        .malloc
        .clone()
        .ok_or_else(|| format_err!("no malloc"))?;
    let ptr = f.call(&mut *caller, size)?;
    ensure!(ptr != 0, "out of memory");
    Ok(ptr)
}

fn free(caller: &mut Ctx, ptr: u32) -> wasmtime::Result<()> {
    let f = caller
        .data()
        .free
        .clone()
        .ok_or_else(|| format_err!("no free"))?;
    f.call(&mut *caller, ptr)
}

fn alloc_std_string(store: &mut Store<Host>, bytes: &[u8]) -> Result<u32, String> {
    let malloc = store.data().malloc.clone().ok_or("no malloc")?;
    let memory = store.data().memory.ok_or("no memory")?;
    let ptr = malloc
        .call(&mut *store, 4 + bytes.len() as u32 + 1)
        .map_err(|e| e.to_string())?;
    let mut block = (bytes.len() as u32).to_le_bytes().to_vec();
    block.extend_from_slice(bytes);
    block.push(0);
    memory
        .write(&mut *store, ptr as usize, &block)
        .map_err(|e| e.to_string())?;
    Ok(ptr)
}

/// embind `readValueFromPointer`: decodes the wire value stored at `ptr`.
fn read_value(caller: &mut Ctx, ty: WireType, ptr: u32) -> wasmtime::Result<Value> {
    Ok(match ty {
        WireType::Void => Value::Undefined,
        WireType::Bool { .. } => Value::Bool(read_array::<1>(caller, ptr)?[0] != 0),
        WireType::Int { size, signed } => Value::Number(match (size, signed) {
            (1, true) => read_array::<1>(caller, ptr)?[0] as i8 as f64,
            (1, false) => read_array::<1>(caller, ptr)?[0] as f64,
            (2, true) => i16::from_le_bytes(read_array(caller, ptr)?) as f64,
            (2, false) => u16::from_le_bytes(read_array(caller, ptr)?) as f64,
            (4, true) => i32::from_le_bytes(read_array(caller, ptr)?) as f64,
            (4, false) => u32::from_le_bytes(read_array(caller, ptr)?) as f64,
            _ => bail!("unsupported integer size {size}"),
        }),
        WireType::Float { size: 4 } => {
            Value::Number(f32::from_le_bytes(read_array(caller, ptr)?) as f64)
        }
        WireType::Float { .. } => Value::Number(f64::from_le_bytes(read_array(caller, ptr)?)),
        WireType::Emval => {
            // fromWireType takes over the reference.
            let handle = read_u32(caller, ptr)?;
            let value = caller.data().value(handle);
            caller.data_mut().decref(handle);
            value
        }
        WireType::String => {
            let wire = read_u32(caller, ptr)?;
            let len = read_u32(caller, wire)?;
            let bytes = read_bytes(caller, wire + 4, len as usize)?;
            free(caller, wire)?;
            Value::Str(Rc::from(String::from_utf8_lossy(&bytes)))
        }
        WireType::MemoryView { kind } => {
            let len = read_u32(caller, ptr)?;
            let data = read_u32(caller, ptr + 4)?;
            Value::MemoryView(kind, data, len)
        }
        WireType::WString | WireType::BigInt => bail!("unsupported embind argument type"),
    })
}

/// embind `toWireType` for return values (`val::as`, method results). Allocations to release
/// later go to `destructors`.
fn to_wire(
    caller: &mut Ctx,
    ty: WireType,
    value: &Value,
    destructors: &mut Vec<u32>,
) -> wasmtime::Result<f64> {
    Ok(match ty {
        WireType::Void => 0.0,
        WireType::Bool { yes, no } => (if value.truthy() { yes } else { no }) as f64,
        WireType::Int { signed: false, .. } => (value.as_number() as i64 as u32) as f64,
        WireType::Int { .. } | WireType::Float { .. } => value.as_number(),
        WireType::Emval => {
            let v = value.clone();
            caller.data_mut().new_handle(v) as f64
        }
        WireType::String => {
            let bytes: Vec<u8> = match value {
                Value::Str(s) => s.as_bytes().to_vec(),
                Value::Bytes(b) => b.to_vec(),
                _ => bail!("cannot pass a non-string to std::string"),
            };
            let ptr = malloc(caller, 4 + bytes.len() as u32 + 1)?;
            let mut block = (bytes.len() as u32).to_le_bytes().to_vec();
            block.extend_from_slice(&bytes);
            block.push(0);
            write(caller, ptr, &block)?;
            destructors.push(ptr);
            ptr as f64
        }
        WireType::WString | WireType::MemoryView { .. } | WireType::BigInt => {
            bail!("unsupported embind return type")
        }
    })
}

fn store_destructors(
    caller: &mut Ctx,
    destructors: Vec<u32>,
    dest_ref: u32,
) -> wasmtime::Result<()> {
    if !destructors.is_empty() {
        let handle = caller
            .data_mut()
            .new_handle(Value::Destructors(Rc::from(destructors)));
        write(caller, dest_ref, &handle.to_le_bytes())?;
    }
    Ok(())
}

/// The few JavaScript methods occt-import-js calls on values.
fn call_method(
    caller: &mut Ctx,
    object: &Value,
    name: &str,
    args: &[Value],
) -> wasmtime::Result<Value> {
    let has_own = |object: &Value, key: &Value| {
        Value::Bool(
            matches!(object, Value::Object(_)) && !matches!(object.property(key), Value::Undefined),
        )
    };
    match (name, object, args) {
        ("hasOwnProperty", _, [key]) => Ok(has_own(object, key)),
        // val::hasOwnProperty: `Object.prototype.hasOwnProperty.call(object, key)`.
        ("call", Value::Builtin("hasOwnProperty"), [this, key]) => Ok(has_own(this, key)),
        // convertJSArrayToNumberVector: `new Uint8Array(heap, ptr, n).set(input)`.
        ("set", Value::MemoryView(1, ptr, len), [Value::Bytes(src)]) => {
            let n = (*len as usize).min(src.len());
            write(caller, *ptr, &src[..n])?;
            Ok(Value::Undefined)
        }
        _ => bail!(
            "unsupported JavaScript method call: {}.{name}({})",
            object.kind(),
            args.iter().map(Value::kind).collect::<Vec<_>>().join(", ")
        ),
    }
}

// --- imports ---

/// `struct tm` (Emscripten layout: 9 ints, `tm_gmtoff`, `tm_zone`) for `seconds` since the
/// epoch, in UTC.
fn utc_tm(seconds: i64) -> [u8; 44] {
    let days = seconds.div_euclid(86_400);
    let secs = seconds.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let mday = doy - (153 * mp + 2) / 5 + 1;
    let mon = if mp < 10 { mp + 3 } else { mp - 9 } - 1;
    let year = yoe + era * 400 + (mon < 2) as i64;
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    const CUMULATIVE: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let yday = CUMULATIVE[mon as usize] + mday - 1 + (leap && mon > 1) as i64;
    let wday = (days + 4).rem_euclid(7);
    let fields = [
        secs % 60,
        secs / 60 % 60,
        secs / 3600,
        mday,
        mon,
        year - 1900,
        wday,
        yday,
        0, // tm_isdst
        0, // tm_gmtoff
        0, // tm_zone
    ];
    let mut out = [0u8; 44];
    for (i, f) in fields.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&(*f as i32).to_le_bytes());
    }
    out
}

const ENOENT: i32 = 44;
const EBADF: i32 = 8;
const EINVAL: i32 = 28;
const ENOTTY: i32 = 59;
const ESPIPE: i32 = 70;
const O_WRONLY: i32 = 0o1;
const O_RDWR: i32 = 0o2;
const O_CREAT: i32 = 0o100;
const O_TRUNC: i32 = 0o1000;
const O_APPEND: i32 = 0o2000;

/// `struct stat` in Emscripten's layout: a regular file of `size` bytes, or a character device
/// for the standard streams.
fn stat(size: Option<usize>) -> [u8; 96] {
    let mut buf = [0u8; 96];
    let mode: u32 = if size.is_some() { 0o100644 } else { 0o020666 };
    buf[4..8].copy_from_slice(&mode.to_le_bytes());
    buf[8..12].copy_from_slice(&1u32.to_le_bytes()); // nlink
    buf[24..32].copy_from_slice(&(size.unwrap_or(0) as u64).to_le_bytes());
    buf[32..36].copy_from_slice(&4096u32.to_le_bytes()); // blksize
    buf[36..40].copy_from_slice(&(size.unwrap_or(0).div_ceil(512) as u32).to_le_bytes());
    buf[88..96].copy_from_slice(&1u64.to_le_bytes()); // ino
    buf
}

struct OpenFile {
    path: String,
    pos: u64,
    flags: i32,
}

#[derive(Default)]
struct MemFs {
    files: HashMap<String, Vec<u8>>,
    open: HashMap<i32, OpenFile>,
}

impl MemFs {
    /// Everything lives in one flat root folder, which is also the working directory.
    fn normalize(path: &str) -> String {
        path.trim_start_matches("./")
            .trim_start_matches('/')
            .to_string()
    }

    fn open(&mut self, path: &str, flags: i32) -> i32 {
        let path = Self::normalize(path);
        if !self.files.contains_key(&path) {
            if flags & O_CREAT == 0 {
                return -ENOENT;
            }
            self.files.insert(path.clone(), Vec::new());
        }
        if flags & O_TRUNC != 0 && flags & (O_WRONLY | O_RDWR) != 0 {
            self.files.insert(path.clone(), Vec::new());
        }
        let fd = (3..).find(|fd| !self.open.contains_key(fd)).unwrap();
        self.open.insert(
            fd,
            OpenFile {
                path,
                pos: 0,
                flags,
            },
        );
        fd
    }

    fn write(&mut self, fd: i32, data: &[u8]) -> Result<(), i32> {
        let f = self.open.get_mut(&fd).ok_or(EBADF)?;
        let file = self.files.get_mut(&f.path).ok_or(EBADF)?;
        if f.flags & O_APPEND != 0 {
            f.pos = file.len() as u64;
        }
        let start = f.pos as usize;
        let end = start + data.len();
        if file.len() < end {
            file.resize(end, 0);
        }
        file[start..end].copy_from_slice(data);
        f.pos = end as u64;
        Ok(())
    }

    fn read(&mut self, fd: i32, len: usize) -> Result<Vec<u8>, i32> {
        if (0..=2).contains(&fd) {
            return Ok(Vec::new());
        }
        let f = self.open.get_mut(&fd).ok_or(EBADF)?;
        let file = self.files.get(&f.path).ok_or(EBADF)?;
        let start = (f.pos as usize).min(file.len());
        let end = (start + len).min(file.len());
        f.pos = end as u64;
        Ok(file[start..end].to_vec())
    }

    fn seek(&mut self, fd: i32, offset: i64, whence: i32) -> Result<i64, i32> {
        let f = self
            .open
            .get_mut(&fd)
            .ok_or(if (0..=2).contains(&fd) { ESPIPE } else { EBADF })?;
        let len = self.files.get(&f.path).map_or(0, Vec::len) as i64;
        let base = match whence {
            0 => 0,
            1 => f.pos as i64,
            2 => len,
            _ => return Err(EINVAL),
        };
        let pos = base + offset;
        if pos < 0 {
            return Err(EINVAL);
        }
        f.pos = pos as u64;
        Ok(pos)
    }
}

fn define_imports(linker: &mut Linker<Host>) -> wasmtime::Result<()> {
    let m = "env";

    // embind type registrations.
    linker.func_wrap(
        m,
        "_embind_register_void",
        |mut c: Ctx, raw: u32, _name: u32| {
            c.data_mut().types.insert(raw, WireType::Void);
        },
    )?;
    linker.func_wrap(
        m,
        "_embind_register_bool",
        |mut c: Ctx, raw: u32, _name: u32, yes: u32, no: u32| {
            c.data_mut().types.insert(raw, WireType::Bool { yes, no });
        },
    )?;
    linker.func_wrap(
        m,
        "_embind_register_integer",
        |mut c: Ctx, raw: u32, _name: u32, size: u32, min: i32, _max: i32| {
            let signed = min != 0;
            c.data_mut()
                .types
                .insert(raw, WireType::Int { size, signed });
        },
    )?;
    linker.func_wrap(
        m,
        "_embind_register_bigint",
        |mut c: Ctx, raw: u32, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32| {
            c.data_mut().types.insert(raw, WireType::BigInt);
        },
    )?;
    linker.func_wrap(
        m,
        "_embind_register_float",
        |mut c: Ctx, raw: u32, _name: u32, size: u32| {
            c.data_mut().types.insert(raw, WireType::Float { size });
        },
    )?;
    linker.func_wrap(
        m,
        "_embind_register_std_string",
        |mut c: Ctx, raw: u32, _name: u32| {
            c.data_mut().types.insert(raw, WireType::String);
        },
    )?;
    linker.func_wrap(
        m,
        "_embind_register_std_wstring",
        |mut c: Ctx, raw: u32, _size: u32, _name: u32| {
            c.data_mut().types.insert(raw, WireType::WString);
        },
    )?;
    linker.func_wrap(m, "_embind_register_emval", |mut c: Ctx, raw: u32| {
        c.data_mut().types.insert(raw, WireType::Emval);
    })?;
    linker.func_wrap(
        m,
        "_embind_register_memory_view",
        |mut c: Ctx, raw: u32, kind: u32, _name: u32| {
            c.data_mut()
                .types
                .insert(raw, WireType::MemoryView { kind });
        },
    )?;
    linker.func_wrap(
        m,
        "_embind_register_function",
        |mut c: Ctx,
         name: u32,
         _argc: u32,
         _arg_types: u32,
         _signature: u32,
         invoker: u32,
         function: u32,
         _is_async: u32,
         _nonnull: u32|
         -> wasmtime::Result<()> {
            let name = read_cstr(&c, name)?;
            c.data_mut()
                .functions
                .insert(name, Function { invoker, function });
            Ok(())
        },
    )?;

    // emval: values.
    linker.func_wrap(m, "_emval_incref", |mut c: Ctx, h: u32| {
        c.data_mut().incref(h)
    })?;
    linker.func_wrap(m, "_emval_decref", |mut c: Ctx, h: u32| {
        c.data_mut().decref(h)
    })?;
    linker.func_wrap(m, "_emval_new_object", |mut c: Ctx| -> u32 {
        c.data_mut()
            .new_handle(Value::Object(Rc::new(RefCell::new(Vec::new()))))
    })?;
    linker.func_wrap(m, "_emval_new_array", |mut c: Ctx| -> u32 {
        c.data_mut()
            .new_handle(Value::Array(Rc::new(RefCell::new(Vec::new()))))
    })?;
    linker.func_wrap(
        m,
        "_emval_new_cstring",
        |mut c: Ctx, ptr: u32| -> wasmtime::Result<u32> {
            let s = read_cstr(&c, ptr)?;
            Ok(c.data_mut().new_handle(Value::str(&s)))
        },
    )?;
    linker.func_wrap(
        m,
        "_emval_take_value",
        |mut c: Ctx, ty: u32, ptr: u32| -> wasmtime::Result<u32> {
            let ty = c.data().wire_type(ty)?;
            let value = read_value(&mut c, ty, ptr)?;
            Ok(c.data_mut().new_handle(value))
        },
    )?;
    linker.func_wrap(
        m,
        "_emval_get_global",
        |mut c: Ctx, name: u32| -> wasmtime::Result<u32> {
            let value = match name {
                0 => Value::Undefined,
                _ => match read_cstr(&c, name)?.as_str() {
                    "Object" => Value::Builtin("Object"),
                    _ => Value::Undefined,
                },
            };
            Ok(c.data_mut().new_handle(value))
        },
    )?;
    linker.func_wrap(
        m,
        "_emval_get_property",
        |mut c: Ctx, obj: u32, key: u32| -> u32 {
            let host = c.data_mut();
            let value = host.value(obj).property(&host.value(key));
            host.new_handle(value)
        },
    )?;
    linker.func_wrap(
        m,
        "_emval_set_property",
        |c: Ctx, obj: u32, key: u32, value: u32| {
            let host = c.data();
            host.value(obj)
                .set_property(host.value(key), host.value(value));
        },
    )?;
    linker.func_wrap(
        m,
        "_emval_as",
        |mut c: Ctx, h: u32, ty: u32, dest_ref: u32| -> wasmtime::Result<f64> {
            let ty = c.data().wire_type(ty)?;
            let value = c.data().value(h);
            let mut destructors = Vec::new();
            let wire = to_wire(&mut c, ty, &value, &mut destructors)?;
            store_destructors(&mut c, destructors, dest_ref)?;
            Ok(wire)
        },
    )?;
    linker.func_wrap(
        m,
        "_emval_run_destructors",
        |mut c: Ctx, h: u32| -> wasmtime::Result<()> {
            if let Value::Destructors(ptrs) = c.data().value(h) {
                for &p in ptrs.iter() {
                    free(&mut c, p)?;
                }
            }
            c.data_mut().decref(h);
            Ok(())
        },
    )?;
    linker.func_wrap(
        m,
        "_emval_get_method_caller",
        |mut c: Ctx, argc: u32, arg_types: u32, _kind: u32| -> wasmtime::Result<u32> {
            ensure!(argc >= 1, "method caller without a return type");
            let mut types = Vec::with_capacity(argc as usize);
            for i in 0..argc {
                types.push(read_u32(&c, arg_types + 4 * i)?);
            }
            let host = c.data_mut();
            host.callers.push(MethodCaller {
                ret: types[0],
                args: types[1..].to_vec(),
            });
            Ok((host.callers.len() - 1) as u32)
        },
    )?;
    linker.func_wrap(
        m,
        "_emval_call_method",
        |mut c: Ctx,
         caller_id: u32,
         obj: u32,
         name: u32,
         dest_ref: u32,
         args: u32|
         -> wasmtime::Result<f64> {
            let (ret, arg_types) = {
                let mc = c
                    .data()
                    .callers
                    .get(caller_id as usize)
                    .ok_or_else(|| format_err!("unknown method caller"))?;
                (mc.ret, mc.args.clone())
            };
            let name = read_cstr(&c, name)?;
            let object = c.data().value(obj);
            let mut values = Vec::with_capacity(arg_types.len());
            for (i, ty) in arg_types.iter().enumerate() {
                // Arguments are packed in 8-byte slots (GenericWireTypeSize).
                let ty = c.data().wire_type(*ty)?;
                values.push(read_value(&mut c, ty, args + 8 * i as u32)?);
            }
            let result = call_method(&mut c, &object, &name, &values)?;
            let ret = c.data().wire_type(ret)?;
            let mut destructors = Vec::new();
            let wire = to_wire(&mut c, ret, &result, &mut destructors)?;
            store_destructors(&mut c, destructors, dest_ref)?;
            Ok(wire)
        },
    )?;

    // Emscripten runtime.
    linker.func_wrap(m, "_abort_js", || -> wasmtime::Result<()> {
        bail!("OpenCascade aborted")
    })?;
    linker.func_wrap(m, "exit", |code: i32| -> wasmtime::Result<()> {
        bail!("OpenCascade exited with code {code}")
    })?;
    linker.func_wrap(m, "emscripten_get_heap_max", || MAX_HEAP as u32)?;
    linker.func_wrap(
        m,
        "emscripten_resize_heap",
        |mut c: Ctx, requested: u32| -> wasmtime::Result<i32> {
            let mem = memory(&c)?;
            let current = mem.data_size(&c) as u64;
            let requested = requested as u64;
            if requested > MAX_HEAP as u64 {
                return Ok(0);
            }
            // Over-allocate like Emscripten's JS glue to avoid growing on every allocation.
            let target = (requested.max(current + current / 5)).min(MAX_HEAP as u64);
            let pages = target.div_ceil(65536).saturating_sub(current / 65536);
            Ok(match mem.grow(&mut c, pages) {
                Ok(_) => 1,
                Err(_) => mem
                    .grow(&mut c, requested.div_ceil(65536) - current / 65536)
                    .is_ok() as i32,
            })
        },
    )?;
    linker.func_wrap(
        m,
        "_emscripten_memcpy_js",
        |mut c: Ctx, dest: u32, src: u32, n: u32| -> wasmtime::Result<()> {
            let mem = memory(&c)?;
            let data = mem.data_mut(&mut c);
            let (src, dest, n) = (src as usize, dest as usize, n as usize);
            ensure!(
                src + n <= data.len() && dest + n <= data.len(),
                "memcpy out of bounds"
            );
            data.copy_within(src..src + n, dest);
            Ok(())
        },
    )?;
    linker.func_wrap(m, "_emscripten_get_now_is_monotonic", || 1i32)?;
    linker.func_wrap(m, "emscripten_date_now", || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    })?;
    linker.func_wrap(m, "emscripten_get_callstack", |_: i32, _: i32, _: i32| 0i32)?;
    linker.func_wrap(m, "_emscripten_lookup_name", |_: i32| 0i32)?;
    linker.func_wrap(
        m,
        "_localtime_js",
        |mut c: Ctx, lo: u32, hi: i32, tm: u32| -> wasmtime::Result<()> {
            // OCCT stamps dates (e.g. in IGES/STEP headers) and rejects an invalid `struct tm`.
            let seconds = ((hi as i64) << 32) | lo as i64;
            write(&mut c, tm, &utc_tm(seconds))
        },
    )?;
    linker.func_wrap(
        m,
        "_tzset_js",
        |mut c: Ctx,
         timezone: u32,
         daylight: u32,
         std_name: u32,
         dst_name: u32|
         -> wasmtime::Result<()> {
            write(&mut c, timezone, &0i32.to_le_bytes())?;
            write(&mut c, daylight, &0i32.to_le_bytes())?;
            write(&mut c, std_name, b"UTC\0")?;
            write(&mut c, dst_name, b"UTC\0")
        },
    )?;
    linker.func_wrap(
        m,
        "_munmap_js",
        |_: i32, _: i32, _: i32, _: i32, _: i32, _: i32, _: i32| 0i32,
    )?;
    linker.func_wrap(
        m,
        "environ_sizes_get",
        |mut c: Ctx, count: u32, size: u32| -> wasmtime::Result<i32> {
            write(&mut c, count, &0u32.to_le_bytes())?;
            write(&mut c, size, &0u32.to_le_bytes())?;
            Ok(0)
        },
    )?;
    linker.func_wrap(m, "environ_get", |_: i32, _: i32| 0i32)?;

    // A tiny in-memory file system: the IGES importer writes the input to a temporary file and
    // has OCCT read it back. Console output (fds 1 and 2) is discarded.
    linker.func_wrap(
        m,
        "fd_write",
        |mut c: Ctx, fd: i32, iov: u32, iovcnt: u32, written: u32| -> wasmtime::Result<i32> {
            let mut data = Vec::new();
            for i in 0..iovcnt {
                let ptr = read_u32(&c, iov + 8 * i)?;
                let len = read_u32(&c, iov + 8 * i + 4)?;
                if fd > 2 {
                    data.extend(read_bytes(&c, ptr, len as usize)?);
                } else {
                    data.resize(data.len() + len as usize, 0);
                }
            }
            if fd > 2 {
                if let Err(errno) = c.data_mut().fs.write(fd, &data) {
                    return Ok(errno);
                }
            }
            write(&mut c, written, &(data.len() as u32).to_le_bytes())?;
            Ok(0)
        },
    )?;
    linker.func_wrap(
        m,
        "fd_read",
        |mut c: Ctx, fd: i32, iov: u32, iovcnt: u32, read: u32| -> wasmtime::Result<i32> {
            let mut total = 0u32;
            for i in 0..iovcnt {
                let ptr = read_u32(&c, iov + 8 * i)?;
                let len = read_u32(&c, iov + 8 * i + 4)?;
                let chunk = match c.data_mut().fs.read(fd, len as usize) {
                    Ok(chunk) => chunk,
                    Err(errno) => return Ok(errno),
                };
                write(&mut c, ptr, &chunk)?;
                total += chunk.len() as u32;
                if chunk.len() < len as usize {
                    break;
                }
            }
            write(&mut c, read, &total.to_le_bytes())?;
            Ok(0)
        },
    )?;
    linker.func_wrap(
        m,
        "fd_seek",
        |mut c: Ctx,
         fd: i32,
         lo: u32,
         hi: i32,
         whence: i32,
         new_offset: u32|
         -> wasmtime::Result<i32> {
            let offset = ((hi as i64) << 32) | lo as i64;
            match c.data_mut().fs.seek(fd, offset, whence) {
                Ok(pos) => {
                    write(&mut c, new_offset, &pos.to_le_bytes())?;
                    Ok(0)
                }
                Err(errno) => Ok(errno),
            }
        },
    )?;
    linker.func_wrap(m, "fd_close", |mut c: Ctx, fd: i32| {
        if c.data_mut().fs.open.remove(&fd).is_some() || (0..=2).contains(&fd) {
            0
        } else {
            EBADF
        }
    })?;
    linker.func_wrap(
        m,
        "__syscall_openat",
        |mut c: Ctx, _dirfd: i32, path: u32, flags: i32, _mode: u32| -> wasmtime::Result<i32> {
            let path = read_cstr(&c, path)?;
            Ok(c.data_mut().fs.open(&path, flags))
        },
    )?;
    linker.func_wrap(
        m,
        "__syscall_fcntl64",
        |c: Ctx, fd: i32, cmd: i32, _arg: u32| -> i32 {
            let fs = &c.data().fs;
            match (fs.open.get(&fd), cmd) {
                (None, _) if !(0..=2).contains(&fd) => -EBADF,
                (Some(f), 3) => f.flags, // F_GETFL
                (None, 3) => 2,          // O_RDWR for the standard streams
                (_, 1 | 2 | 4 | 13 | 14) => 0,
                _ => -EINVAL,
            }
        },
    )?;
    linker.func_wrap(m, "__syscall_ioctl", |_: i32, _: i32, _: i32| -ENOTTY)?;
    linker.func_wrap(
        m,
        "__syscall_fstat64",
        |mut c: Ctx, fd: i32, buf: u32| -> wasmtime::Result<i32> {
            let size = match c.data().fs.open.get(&fd) {
                Some(f) => Some(c.data().fs.files.get(&f.path).map_or(0, Vec::len)),
                None if (0..=2).contains(&fd) => None,
                None => return Ok(-EBADF),
            };
            write(&mut c, buf, &stat(size))?;
            Ok(0)
        },
    )?;
    fn stat_path(mut c: Ctx, path: u32, buf: u32) -> wasmtime::Result<i32> {
        let path = MemFs::normalize(&read_cstr(&c, path)?);
        let Some(size) = c.data().fs.files.get(&path).map(Vec::len) else {
            return Ok(-ENOENT);
        };
        write(&mut c, buf, &stat(Some(size)))?;
        Ok(0)
    }
    linker.func_wrap(m, "__syscall_stat64", stat_path)?;
    linker.func_wrap(m, "__syscall_lstat64", stat_path)?;
    linker.func_wrap(
        m,
        "__syscall_newfstatat",
        |c: Ctx, _dirfd: i32, path: u32, buf: u32, _flags: i32| stat_path(c, path, buf),
    )?;
    linker.func_wrap(
        m,
        "__syscall_faccessat",
        |c: Ctx, _dirfd: i32, path: u32, _mode: i32, _flags: i32| -> wasmtime::Result<i32> {
            let path = MemFs::normalize(&read_cstr(&c, path)?);
            Ok(if c.data().fs.files.contains_key(&path) {
                0
            } else {
                -ENOENT
            })
        },
    )?;
    linker.func_wrap(m, "__syscall_chmod", |_: i32, _: i32| 0)?;
    linker.func_wrap(m, "__syscall_rmdir", |_: i32| -ENOENT)?;
    linker.func_wrap(
        m,
        "__syscall_unlinkat",
        |mut c: Ctx, _dirfd: i32, path: u32, _flags: i32| -> wasmtime::Result<i32> {
            let path = MemFs::normalize(&read_cstr(&c, path)?);
            Ok(if c.data_mut().fs.files.remove(&path).is_some() {
                0
            } else {
                -ENOENT
            })
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::utc_tm;

    fn fields(t: [u8; 44]) -> Vec<i32> {
        t.chunks(4)
            .map(|c| i32::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    #[test]
    fn utc_tm_matches_known_dates() {
        // 2024-02-29T13:45:30Z, a Thursday, day 59 of a leap year.
        assert_eq!(
            &fields(utc_tm(1_709_214_330))[..8],
            &[30, 45, 13, 29, 1, 124, 4, 59]
        );
        // 1970-01-01T00:00:00Z, a Thursday.
        assert_eq!(&fields(utc_tm(0))[..8], &[0, 0, 0, 1, 0, 70, 4, 0]);
        // 2023-12-31T23:59:59Z, a Sunday, day 364.
        assert_eq!(
            &fields(utc_tm(1_704_067_199))[..8],
            &[59, 59, 23, 31, 11, 123, 0, 364]
        );
    }
}
