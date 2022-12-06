use crate::debugid_util::debug_id_for_object;
use crate::dwarf::{get_frames, Addr2lineContextData};
use crate::error::Error;
use crate::path_mapper::PathMapper;
use crate::shared::{
    BasePath, ExternalFileAddressRef, ExternalFileRef, FileAndPathHelper, FileContents,
    FileContentsWrapper, FileLocation, RangeReadRef, SymbolMapTypeErasedOwned,
};
use crate::symbol_map_object::{
    FunctionAddressesComputer, GenericSymbolMap, ObjectData, ObjectWrapperTrait, SymbolDataTrait,
};
use crate::InlineStackFrame;
use debugid::DebugId;
use macho_unwind_info::UnwindInfo;
use object::macho::{self, LinkeditDataCommand, MachHeader32, MachHeader64};
use object::read::macho::{FatArch, LoadCommandIterator, MachHeader};
use object::read::{archive::ArchiveFile, File, Object, ObjectSection, ObjectSymbol};
use object::{Endianness, ReadRef};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use yoke::{Yoke, Yokeable};

/// Returns the (offset, size) in the fat binary file for the object that matches
// breakpad_id, if found.
pub fn get_arch_range(
    file_contents: &FileContentsWrapper<impl FileContents>,
    arches: &[impl FatArch],
    debug_id: DebugId,
) -> Result<(u64, u64), Error> {
    let mut debug_ids = Vec::new();
    let mut errors = Vec::new();

    for fat_arch in arches {
        let range = fat_arch.file_range();
        let (start, size) = range;
        let file =
            File::parse(file_contents.range(start, size)).map_err(Error::MachOHeaderParseError)?;
        match debug_id_for_object(&file) {
            Some(di) => {
                if di == debug_id {
                    return Ok(range);
                }
                debug_ids.push(di);
            }
            None => {
                errors.push(Error::InvalidInputError("Missing mach-O UUID"));
            }
        }
    }
    Err(Error::NoMatchMultiArch(debug_ids, errors))
}

pub async fn get_symbol_map_for_dyld_cache<'h, H>(
    dyld_cache_path: &Path,
    dylib_path: &str,
    helper: &'h H,
) -> Result<SymbolMapTypeErasedOwned, Error>
where
    H: FileAndPathHelper<'h>,
{
    let get_file = |path| helper.open_file(&FileLocation::Path(path));

    let root_contents = get_file(dyld_cache_path.into()).await.map_err(|e| {
        Error::HelperErrorDuringOpenFile(dyld_cache_path.to_string_lossy().to_string(), e)
    })?;
    let root_contents = FileContentsWrapper::new(root_contents);

    let dyld_cache_path = dyld_cache_path.to_string_lossy();

    let mut subcache_contents = Vec::new();
    for subcache_index in 1.. {
        // Find the subcache at dyld_shared_cache_arm64e.1 or dyld_shared_cache_arm64e.01
        let subcache_path = format!("{}.{}", dyld_cache_path, subcache_index);
        let subcache_path2 = format!("{}.{:02}", dyld_cache_path, subcache_index);
        let subcache = match get_file(subcache_path.into()).await {
            Ok(subcache) => subcache,
            Err(_) => match get_file(subcache_path2.into()).await {
                Ok(subcache) => subcache,
                Err(_) => break,
            },
        };
        subcache_contents.push(FileContentsWrapper::new(subcache));
    }
    let symbols_subcache_path = format!("{}.symbols", dyld_cache_path);
    if let Ok(subcache) = get_file(symbols_subcache_path.into()).await {
        subcache_contents.push(FileContentsWrapper::new(subcache));
    };

    let base_path = BasePath::CanReferToLocalFiles(PathBuf::from(dylib_path));
    let owner = DyldCacheSymbolMapData::new(root_contents, subcache_contents, dylib_path);
    let symbol_map = GenericSymbolMap::new(owner, &base_path)?;
    Ok(SymbolMapTypeErasedOwned(Box::new(symbol_map)))
}

struct DyldCacheSymbolMapData<T>
where
    T: FileContents + 'static,
{
    root_file_data: FileContentsWrapper<T>,
    subcache_file_data: Vec<FileContentsWrapper<T>>,
    dylib_path: String,
}

impl<T: FileContents + 'static> DyldCacheSymbolMapData<T> {
    pub fn new(
        root_file_data: FileContentsWrapper<T>,
        subcache_file_data: Vec<FileContentsWrapper<T>>,
        dylib_path: &str,
    ) -> Self {
        Self {
            root_file_data,
            subcache_file_data,
            dylib_path: dylib_path.to_string(),
        }
    }
}

impl<T: FileContents + 'static> SymbolDataTrait for DyldCacheSymbolMapData<T> {
    fn make_object_wrapper(&self) -> Result<Box<dyn ObjectWrapperTrait + '_>, Error> {
        let subcache_contents_refs: Vec<_> = self.subcache_file_data.iter().collect();
        let cache = object::read::macho::DyldCache::<Endianness, _>::parse(
            &self.root_file_data,
            &subcache_contents_refs,
        )
        .map_err(Error::DyldCacheParseError)?;
        let image = match cache
            .images()
            .find(|image| image.path() == Ok(&self.dylib_path))
        {
            Some(image) => image,
            None => return Err(Error::NoMatchingDyldCacheImagePath(self.dylib_path.clone())),
        };

        let object = image.parse_object().map_err(Error::MachOHeaderParseError)?;

        let (data, header_offset) = image
            .image_data_and_offset()
            .map_err(Error::MachOHeaderParseError)?;
        let macho_data = MachOData::new(data, header_offset, object.is_64());
        let function_addresses_computer = MachOFunctionAddressesComputer { macho_data };

        let object = ObjectData::new(object, function_addresses_computer, &self.root_file_data);

        Ok(Box::new(object))
    }
}

pub fn get_symbol_map<F: FileContents + 'static>(
    base_path: &BasePath,
    file_contents: FileContentsWrapper<F>,
) -> Result<SymbolMapTypeErasedOwned, Error> {
    let owner = MachSymbolMapData::new(file_contents);
    let symbol_map = GenericSymbolMap::new(owner, base_path)?;
    Ok(SymbolMapTypeErasedOwned(Box::new(symbol_map)))
}

pub fn get_symbol_map_for_fat_archive_member<F: FileContents + 'static>(
    base_path: &BasePath,
    file_contents: FileContentsWrapper<F>,
    file_range: (u64, u64),
) -> Result<SymbolMapTypeErasedOwned, Error> {
    let owner = MachFatArchiveSymbolMapData::new(file_contents, file_range);
    let symbol_map = GenericSymbolMap::new(owner, base_path)?;
    Ok(SymbolMapTypeErasedOwned(Box::new(symbol_map)))
}

struct MachSymbolMapData<T>
where
    T: FileContents,
{
    file_data: FileContentsWrapper<T>,
}

impl<T: FileContents> MachSymbolMapData<T> {
    pub fn new(file_data: FileContentsWrapper<T>) -> Self {
        Self { file_data }
    }
}

impl<T: FileContents + 'static> SymbolDataTrait for MachSymbolMapData<T> {
    fn make_object_wrapper(&self) -> Result<Box<dyn ObjectWrapperTrait + '_>, Error> {
        let macho_file = File::parse(&self.file_data).map_err(Error::MachOHeaderParseError)?;
        let macho_data = MachOData::new(&self.file_data, 0, macho_file.is_64());
        let function_addresses_computer = MachOFunctionAddressesComputer { macho_data };
        let object = ObjectData::new(macho_file, function_addresses_computer, &self.file_data);
        Ok(Box::new(object))
    }
}

struct MachFatArchiveSymbolMapData<T>
where
    T: FileContents,
{
    file_data: FileContentsWrapper<T>,
    file_range: (u64, u64),
}

impl<T: FileContents> MachFatArchiveSymbolMapData<T> {
    pub fn new(file_data: FileContentsWrapper<T>, file_range: (u64, u64)) -> Self {
        Self {
            file_data,
            file_range,
        }
    }
}

impl<T: FileContents + 'static> SymbolDataTrait for MachFatArchiveSymbolMapData<T> {
    fn make_object_wrapper(&self) -> Result<Box<dyn ObjectWrapperTrait + '_>, Error> {
        let file_contents_ref = &self.file_data;
        let (start, size) = self.file_range;
        let range_data = file_contents_ref.range(start, size);
        let macho_file = File::parse(range_data).map_err(Error::MachOHeaderParseError)?;
        let macho_data = MachOData::new(range_data, 0, macho_file.is_64());
        let function_addresses_computer = MachOFunctionAddressesComputer { macho_data };
        let object = ObjectData::new(macho_file, function_addresses_computer, range_data);
        Ok(Box::new(object))
    }
}

/// Get a list of function addresses as u32 relative addresses.
pub fn function_start_addresses<'data: 'file, 'file, T, R: ReadRef<'data>>(
    object_file: &'file T,
    macho_data: &'file MachOData<'data, R>,
) -> Option<Vec<u32>>
where
    T: object::Object<'data, 'file>,
{
    // Get function start addresses from LC_FUNCTION_STARTS
    let mut function_starts = macho_data.get_function_starts().ok()?;

    // and from __unwind_info.
    if let Some(unwind_info) = object_file
        .section_by_name_bytes(b"__unwind_info")
        .and_then(|s| s.data().ok())
        .and_then(|d| UnwindInfo::parse(d).ok())
    {
        let function_starts = function_starts.get_or_insert_with(Vec::new);
        let mut iter = unwind_info.functions();
        while let Ok(Some(function)) = iter.next() {
            function_starts.push(function.start_address);
        }
    }
    function_starts
}

pub async fn get_external_file<'h, H, F>(
    helper: &'h H,
    external_file_ref: &ExternalFileRef,
) -> Result<ExternalFileWithUplooker<F>, Error>
where
    F: FileContents + 'static,
    H: FileAndPathHelper<'h, F = F>,
{
    let file = helper
        .open_file(&FileLocation::Path(
            external_file_ref.file_name.as_str().into(),
        ))
        .await
        .map_err(|e| Error::HelperErrorDuringOpenFile(external_file_ref.file_name.clone(), e))?;
    Ok(ExternalFileWithUplooker::new(
        &external_file_ref.file_name,
        file,
    ))
}

// Disabled due to "higher-ranked lifetime error"
#[cfg(any())]
#[test]
fn test_future_send() {
    fn assert_is_send<T: Send>(_f: T) {}
    fn wrapper<'a, 'b, F, H, R>(
        base_path: &BasePath,
        file_contents: FileContentsWrapper<F>,
        file_range: Option<(u64, u64)>,
        query: SymbolicationQuery<'a>,
        helper: &'static H,
    ) where
        F: FileContents + Send + Sync,
        H: FileAndPathHelper<'static, F = F>,
        R: SymbolicationResult + Send,
        <H as FileAndPathHelper<'static>>::OpenFileFuture: Send,
        H: Sync,
    {
        let f = get_symbolication_result::<F, H, R>(
            base_path,
            file_contents,
            file_range,
            query,
            helper,
        );
        assert_is_send(f);
    }
}

struct ExternalObjectUplooker<'a> {
    context: Option<addr2line::Context<gimli::EndianSlice<'a, gimli::RunTimeEndian>>>,
    symbol_addresses: HashMap<&'a [u8], u64>,
}

impl<'a> ExternalObjectUplooker<'a> {
    pub fn lookup_address(
        &self,
        symbol_name: &[u8],
        offset_from_symbol: u32,
        path_mapper: &mut PathMapper<()>,
    ) -> Option<Vec<InlineStackFrame>> {
        let symbol_address = self.symbol_addresses.get(symbol_name)?;
        let address = symbol_address + offset_from_symbol as u64;
        get_frames(address, self.context.as_ref(), path_mapper)
    }
}

struct ExternalFileUplooker<'a, F: FileContents> {
    external_file: &'a ExternalFile<F>,
    object_uplookers: Mutex<HashMap<String, ExternalObjectUplooker<'a>>>,
    path_mapper: Mutex<PathMapper<()>>,
}

struct ExternalFile<F: FileContents> {
    name: String,
    file_contents: FileContentsWrapper<F>,
    base_path: BasePath,
    /// name in bytes -> (start, size) in file_contents
    archive_members_by_name: HashMap<Vec<u8>, (u64, u64)>,
    addr2line_context_data: Addr2lineContextData,
}

trait ExternalFileTrait {
    #[cfg(feature = "send_futures")]
    fn make_type_erased_uplooker(&self) -> Box<dyn ExternalFileUplookerTrait + '_ + Send + Sync>;
    #[cfg(not(feature = "send_futures"))]
    fn make_type_erased_uplooker(&self) -> Box<dyn ExternalFileUplookerTrait + '_>;

    fn make_object_uplooker<'s>(
        &'s self,
        name_in_archive: Option<&str>,
    ) -> Result<ExternalObjectUplooker<'s>, Error>;
    fn name(&self) -> &str;
}

trait ExternalFileUplookerTrait {
    fn lookup_address(
        &self,
        external_file_address: &ExternalFileAddressRef,
    ) -> Option<Vec<InlineStackFrame>>;
}

impl<'a, F: FileContents> ExternalFileUplookerTrait for ExternalFileUplooker<'a, F> {
    fn lookup_address(
        &self,
        external_file_address: &ExternalFileAddressRef,
    ) -> Option<Vec<InlineStackFrame>> {
        let member_key = external_file_address
            .name_in_archive
            .as_deref()
            .unwrap_or("");
        let mut uplookers = self.object_uplookers.lock().unwrap();
        let mut path_mapper = self.path_mapper.lock().unwrap();
        match uplookers.get(member_key) {
            Some(uplooker) => uplooker.lookup_address(
                &external_file_address.symbol_name,
                external_file_address.offset_from_symbol,
                &mut path_mapper,
            ),
            None => {
                let uplooker = self
                    .external_file
                    .make_object_uplooker(external_file_address.name_in_archive.as_deref())
                    .ok()?;
                let res = uplooker.lookup_address(
                    &external_file_address.symbol_name,
                    external_file_address.offset_from_symbol,
                    &mut path_mapper,
                );
                uplookers.insert(member_key.to_string(), uplooker);
                res
            }
        }
    }
}

pub struct ExternalFileWithUplooker<F: FileContents>(
    Yoke<ExternalFileUplookerTypeErased<'static>, Box<ExternalFile<F>>>,
);

impl<F: FileContents> ExternalFileWithUplooker<F> {
    pub fn new(file_name: &str, file: F) -> Self {
        let external_file = Box::new(ExternalFile::new(file_name, file));
        let inner =
            Yoke::<ExternalFileUplookerTypeErased<'static>, Box<ExternalFile<F>>>::attach_to_cart(
                external_file,
                |external_file| {
                    let uplooker = external_file.make_type_erased_uplooker();
                    ExternalFileUplookerTypeErased(uplooker)
                },
            );
        Self(inner)
    }

    pub fn name(&self) -> &str {
        self.0.backing_cart().name()
    }

    pub fn lookup_address(
        &self,
        external_file_address: &ExternalFileAddressRef,
    ) -> Option<Vec<InlineStackFrame>> {
        self.0.get().0.lookup_address(external_file_address)
    }
}

#[cfg(feature = "send_futures")]
#[derive(Yokeable)]
struct ExternalFileUplookerTypeErased<'a>(Box<dyn ExternalFileUplookerTrait + 'a + Send + Sync>);

#[cfg(not(feature = "send_futures"))]
#[derive(Yokeable)]
struct ExternalFileUplookerTypeErased<'a>(Box<dyn ExternalFileUplookerTrait + 'a>);

impl<F: FileContents> ExternalFileTrait for ExternalFile<F> {
    #[cfg(feature = "send_futures")]
    fn make_type_erased_uplooker(&self) -> Box<dyn ExternalFileUplookerTrait + '_ + Send + Sync> {
        Box::new(self.make_uplooker())
    }
    #[cfg(not(feature = "send_futures"))]
    fn make_type_erased_uplooker(&self) -> Box<dyn ExternalFileUplookerTrait + '_> {
        Box::new(self.make_uplooker())
    }
    fn make_object_uplooker<'s>(
        &'s self,
        name_in_archive: Option<&str>,
    ) -> Result<ExternalObjectUplooker<'s>, Error> {
        self.make_object_uplooker_impl(name_in_archive)
    }
    fn name(&self) -> &str {
        &self.name
    }
}

impl<F: FileContents> ExternalFile<F> {
    pub fn new(file_name: &str, file: F) -> Self {
        let base_path = BasePath::CanReferToLocalFiles(PathBuf::from(file_name));
        let file_contents = FileContentsWrapper::new(file);
        let archive_members_by_name: HashMap<Vec<u8>, (u64, u64)> =
            match ArchiveFile::parse(&file_contents) {
                Ok(archive) => archive
                    .members()
                    .filter_map(|member| match member {
                        Ok(member) => Some((member.name().to_owned(), member.file_range())),
                        Err(_) => None,
                    })
                    .collect(),
                Err(_) => HashMap::new(),
            };
        Self {
            name: file_name.to_owned(),
            file_contents,
            base_path,
            archive_members_by_name,
            addr2line_context_data: Addr2lineContextData::new(),
        }
    }

    fn get_archive_member<'s>(
        &'s self,
        name_in_archive: Option<&str>,
    ) -> Result<
        (
            RangeReadRef<'s, &'s FileContentsWrapper<F>>,
            File<'s, RangeReadRef<'s, &'s FileContentsWrapper<F>>>,
        ),
        Error,
    > {
        let data = &self.file_contents;
        let data = match name_in_archive {
            Some(name_in_archive) => {
                let (start, size) = self
                    .archive_members_by_name
                    .get(name_in_archive.as_bytes())
                    .ok_or_else(|| Error::FileNotInArchive(name_in_archive.to_owned()))?;
                RangeReadRef::new(data, *start, *size)
            }
            None => RangeReadRef::new(data, 0, data.len()),
        };
        let object_file = File::parse(data).map_err(Error::MachOHeaderParseError)?;
        Ok((data, object_file))
    }

    pub fn make_object_uplooker_impl<'s>(
        &'s self,
        name_in_archive: Option<&str>,
    ) -> Result<ExternalObjectUplooker<'s>, Error> {
        let (data, object_file) = self.get_archive_member(name_in_archive)?;
        let context = self.addr2line_context_data.make_context(data, &object_file);
        let symbol_addresses = object_file
            .symbols()
            .filter_map(|symbol| {
                let name = symbol.name_bytes().ok()?;
                let address = symbol.address();
                Some((name, address))
            })
            .collect();
        let uplooker = ExternalObjectUplooker {
            context: context.ok(),
            symbol_addresses,
        };
        Ok(uplooker)
    }

    pub fn make_uplooker(&self) -> ExternalFileUplooker<'_, F> {
        let path_mapper = PathMapper::new(&self.base_path);
        ExternalFileUplooker {
            external_file: self,
            object_uplookers: Mutex::new(HashMap::new()),
            path_mapper: Mutex::new(path_mapper),
        }
    }
}

struct MachOFunctionAddressesComputer<'data, R: ReadRef<'data>> {
    macho_data: MachOData<'data, R>,
}

impl<'data, R: ReadRef<'data>> FunctionAddressesComputer<'data>
    for MachOFunctionAddressesComputer<'data, R>
{
    fn compute_function_addresses<'file, O>(
        &'file self,
        object_file: &'file O,
    ) -> (Option<Vec<u32>>, Option<Vec<u32>>)
    where
        'data: 'file,
        O: object::Object<'data, 'file>,
    {
        (
            function_start_addresses(object_file, &self.macho_data),
            None,
        )
    }
}

pub struct MachOData<'data, R: ReadRef<'data>> {
    data: R,
    header_offset: u64,
    is_64: bool,
    _phantom: PhantomData<&'data ()>,
}

impl<'data, R: ReadRef<'data>> MachOData<'data, R> {
    pub fn new(data: R, header_offset: u64, is_64: bool) -> Self {
        Self {
            data,
            header_offset,
            is_64,
            _phantom: PhantomData,
        }
    }

    /// Read the list of function start addresses from the LC_FUNCTION_STARTS mach-O load command.
    /// This information is usually present even in stripped binaries. It's a uleb128 encoded list
    /// of deltas between the function addresses, with a zero delta terminator.
    /// We use this information to improve symbolication for stripped binaries: It allows us to
    /// group addresses from the same function into the same (synthesized) "symbol". It also allows
    /// better results for binaries with partial symbol tables, because it tells us where the
    /// functions with symbols end. This means that those symbols don't "overreach" to cover
    /// addresses after their function - instead, they get correctly terminated by a symbol-less
    /// function's start address.
    pub fn get_function_starts(&self) -> Result<Option<Vec<u32>>, Error> {
        let data = self
            .function_start_data()
            .map_err(Error::MachOHeaderParseError)?;
        let data = if let Some(data) = data {
            data
        } else {
            return Ok(None);
        };
        let mut function_starts = Vec::new();
        let mut prev_address = 0;
        let mut bytes = data;
        while let Some((delta, rest)) = read_uleb128(bytes) {
            if delta == 0 {
                break;
            }
            bytes = rest;
            let address = prev_address + delta;
            function_starts.push(address as u32);
            prev_address = address;
        }

        Ok(Some(function_starts))
    }

    fn load_command_iter<M: MachHeader>(
        &self,
    ) -> object::read::Result<(M::Endian, LoadCommandIterator<M::Endian>)> {
        let header = M::parse(self.data, self.header_offset)?;
        let endian = header.endian()?;
        let load_commands = header.load_commands(endian, self.data, self.header_offset)?;
        Ok((endian, load_commands))
    }

    fn function_start_data(&self) -> object::read::Result<Option<&'data [u8]>> {
        let (endian, mut commands) = if self.is_64 {
            self.load_command_iter::<MachHeader64<Endianness>>()?
        } else {
            self.load_command_iter::<MachHeader32<Endianness>>()?
        };
        while let Ok(Some(command)) = commands.next() {
            if command.cmd() == macho::LC_FUNCTION_STARTS {
                let command: &LinkeditDataCommand<_> = command.data()?;
                let dataoff: u64 = command.dataoff.get(endian).into();
                let datasize: u64 = command.datasize.get(endian).into();
                let data = self.data.read_bytes_at(dataoff, datasize).ok();
                return Ok(data);
            }
        }
        Ok(None)
    }
}

fn read_uleb128(mut bytes: &[u8]) -> Option<(u64, &[u8])> {
    const CONTINUATION_BIT: u8 = 1 << 7;

    let mut result = 0;
    let mut shift = 0;

    while !bytes.is_empty() {
        let byte = bytes[0];
        bytes = &bytes[1..];
        if shift == 63 && byte != 0x00 && byte != 0x01 {
            return None;
        }

        let low_bits = u64::from(byte & !CONTINUATION_BIT);
        result |= low_bits << shift;

        if byte & CONTINUATION_BIT == 0 {
            return Some((result, bytes));
        }

        shift += 7;
    }
    None
}
