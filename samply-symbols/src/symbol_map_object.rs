use std::marker::PhantomData;
use std::sync::Arc;
use std::{borrow::Cow, slice, sync::Mutex};

use addr2line::{LookupResult, SplitDwarfLoad};
use debugid::DebugId;
use gimli::{EndianSlice, RunTimeEndian};
use object::{
    ObjectMap, ObjectSection, ObjectSegment, SectionFlags, SectionIndex, SectionKind, SymbolKind,
};
use yoke::Yoke;
use yoke_derive::Yokeable;

use crate::dwarf::convert_frames;
use crate::path_mapper::PathMapper;
use crate::shared::{
    relative_address_base, AddressInfo, DwoRef, ExternalFileAddressInFileRef,
    ExternalFileAddressRef, ExternalFileRef, FramesLookupResult, SymbolInfo,
};
use crate::symbol_map::{
    FramesLookupResult2, GetInnerSymbolMap, GetInnerSymbolMapWithLookupFramesExt, SymbolMapTrait,
    SymbolMapTraitWithLookupFramesExt,
};
use crate::{demangle, Error, FileContents};

enum FullSymbolListEntry<'a, Symbol: object::ObjectSymbol<'a>> {
    /// A synthesized symbol for a function start address that's known
    /// from some other information (not from the symbol table).
    Synthesized,
    /// A synthesized symbol for the entry point of the object.
    SynthesizedEntryPoint,
    Symbol(Symbol),
    Export(object::Export<'a>),
    EndAddress,
}

impl<'a, Symbol: object::ObjectSymbol<'a>> std::fmt::Debug for FullSymbolListEntry<'a, Symbol> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Synthesized => write!(f, "Synthesized"),
            Self::SynthesizedEntryPoint => write!(f, "SynthesizedEntryPoint"),
            Self::Symbol(arg0) => f
                .debug_tuple("Symbol")
                .field(&arg0.name().unwrap())
                .finish(),
            Self::Export(arg0) => f
                .debug_tuple("Export")
                .field(&std::str::from_utf8(arg0.name()).unwrap())
                .finish(),
            Self::EndAddress => write!(f, "EndAddress"),
        }
    }
}

impl<'a, Symbol: object::ObjectSymbol<'a>> FullSymbolListEntry<'a, Symbol> {
    fn name(&self, addr: u32) -> Result<Cow<'a, str>, ()> {
        match self {
            FullSymbolListEntry::Synthesized => Ok(format!("fun_{addr:x}").into()),
            FullSymbolListEntry::SynthesizedEntryPoint => Ok("EntryPoint".into()),
            FullSymbolListEntry::Symbol(symbol) => match symbol.name_bytes() {
                Ok(name) => Ok(String::from_utf8_lossy(name)),
                Err(_) => Err(()),
            },
            FullSymbolListEntry::Export(export) => Ok(String::from_utf8_lossy(export.name())),
            FullSymbolListEntry::EndAddress => Err(()),
        }
    }
}

// A file range in an object file, such as a segment or a section,
// for which we know the corresponding Stated Virtual Memory Address (SVMA).
#[derive(Clone)]
struct SvmaFileRange {
    svma: u64,
    file_offset: u64,
    size: u64,
}

impl SvmaFileRange {
    pub fn from_segment<'data, S: ObjectSegment<'data>>(segment: S) -> Self {
        let svma = segment.address();
        let (file_offset, size) = segment.file_range();
        SvmaFileRange {
            svma,
            file_offset,
            size,
        }
    }

    pub fn from_section<'data, S: ObjectSection<'data>>(section: S) -> Option<Self> {
        let svma = section.address();
        let (file_offset, size) = section.file_range()?;
        Some(SvmaFileRange {
            svma,
            file_offset,
            size,
        })
    }
}

impl std::fmt::Debug for SvmaFileRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SvmaFileRange")
            .field("svma", &format!("{:#x}", &self.svma))
            .field("file_offset", &format!("{:#x}", &self.file_offset))
            .field("size", &format!("{:#x}", &self.size))
            .finish()
    }
}

pub struct ObjectSymbolMapInnerImpl<'a, Symbol: object::ObjectSymbol<'a>> {
    entries: Vec<(u32, FullSymbolListEntry<'a, Symbol>)>,
    debug_id: DebugId,
    path_mapper: Mutex<PathMapper<()>>,
    object_map: ObjectMap<'a>,
    context: Option<addr2line::Context<gimli::EndianSlice<'a, gimli::RunTimeEndian>>>,
    svma_file_ranges: Vec<SvmaFileRange>,
    image_base_address: u64,
}

pub trait ObjectSymbolMapOuter {
    fn make_symbol_map_inner(&self) -> Result<ObjectSymbolMapInner<'_>, Error>;
}

#[derive(Yokeable)]
pub struct ObjectSymbolMapInner<'data>(pub Box<dyn SymbolMapTrait + Send + 'data>);

pub struct ObjectSymbolMap<OSMO: ObjectSymbolMapOuter>(
    Yoke<ObjectSymbolMapInner<'static>, Box<OSMO>>,
);

impl<OSMO: ObjectSymbolMapOuter + 'static> ObjectSymbolMap<OSMO> {
    pub fn new(outer: OSMO) -> Result<Self, Error> {
        let outer_and_inner =
            Yoke::<ObjectSymbolMapInner, _>::try_attach_to_cart(Box::new(outer), |outer| {
                outer.make_symbol_map_inner()
            })?;
        Ok(ObjectSymbolMap(outer_and_inner))
    }
}

impl<OSMO: ObjectSymbolMapOuter> GetInnerSymbolMap for ObjectSymbolMap<OSMO> {
    fn get_inner_symbol_map<'a>(&'a self) -> &'a (dyn SymbolMapTrait + 'a) {
        self.0.get().0.as_ref()
    }
}

#[test]
fn test_symbolmap_is_send() {
    fn assert_is_send<T: Send>() {}
    assert_is_send::<ObjectSymbolMapInner<'static>>();
}

impl<'a> ObjectSymbolMapInner<'a> {
    pub fn new<'file, O, Symbol: object::ObjectSymbol<'a> + Send + 'a>(
        object_file: &'file O,
        addr2line_context: Option<addr2line::Context<EndianSlice<'a, RunTimeEndian>>>,
        debug_id: DebugId,
        function_start_addresses: Option<&[u32]>,
        function_end_addresses: Option<&[u32]>,
    ) -> Self
    where
        'a: 'file,
        O: object::Object<'a, 'file, Symbol = Symbol>,
    {
        let inner_impl = ObjectSymbolMapInnerImpl::new(
            object_file,
            addr2line_context,
            debug_id,
            function_start_addresses,
            function_end_addresses,
        );
        ObjectSymbolMapInner(Box::new(inner_impl))
    }
}

impl<'a, Symbol: object::ObjectSymbol<'a>> ObjectSymbolMapInnerImpl<'a, Symbol> {
    pub fn new<'file, O>(
        object_file: &'file O,
        addr2line_context: Option<addr2line::Context<EndianSlice<'a, RunTimeEndian>>>,
        debug_id: DebugId,
        function_start_addresses: Option<&[u32]>,
        function_end_addresses: Option<&[u32]>,
    ) -> Self
    where
        'a: 'file,
        O: object::Object<'a, 'file, Symbol = Symbol>,
        Symbol: object::ObjectSymbol<'a> + Send + 'a,
    {
        let mut entries: Vec<_> = Vec::new();

        let base_address = relative_address_base(object_file);

        // Compute the executable sections upfront. This will be used to filter out uninteresting symbols.
        let executable_sections: Vec<SectionIndex> = object_file
            .sections()
            .filter_map(|section| match (section.kind(), section.flags()) {
                // Match executable sections.
                (SectionKind::Text, _) => Some(section.index()),

                // Match sections in debug files which correspond to executable sections in the original binary.
                // "SectionKind::EmptyButUsedToBeText"
                (SectionKind::UninitializedData, SectionFlags::Elf { sh_flags })
                    if sh_flags & u64::from(object::elf::SHF_EXECINSTR) != 0 =>
                {
                    Some(section.index())
                }

                _ => None,
            })
            .collect();

        // Build a list of symbol start and end entries. We add entries in the order "best to worst".

        // 1. Normal symbols
        // 2. Dynamic symbols (only used by ELF files, I think)
        entries.extend(
            object_file
                .symbols()
                .chain(object_file.dynamic_symbols())
                .filter(|symbol| {
                    // Filter out symbols with no address.
                    if symbol.address() == 0 {
                        return false;
                    }

                    // Filter out non-Text symbols which don't have a symbol size.
                    match symbol.kind() {
                        SymbolKind::Text => {
                            // Keep. This is a regular function symbol. On mach-O these don't have sizes.
                        }
                        SymbolKind::Label if symbol.size() != 0 => {
                            // Keep. This catches some useful kernel symbols, e.g. asm_exc_page_fault,
                            // which is a NOTYPE symbol (= SymbolKind::Label).
                            //
                            // We require a non-zero symbol size in this case, in order to filter out some
                            // bad symbols in the middle of functions. For example, the android32-local/libmozglue.so
                            // fixture has a NOTYPE symbol with zero size at 0x9850f.
                        }
                        _ => return false, // Cull.
                    }

                    // Filter out symbols from non-executable sections.
                    match symbol.section_index() {
                        Some(section_index) => executable_sections.contains(&section_index),
                        _ => false,
                    }
                })
                .filter_map(|symbol| {
                    Some((
                        u32::try_from(symbol.address().checked_sub(base_address)?).ok()?,
                        FullSymbolListEntry::Symbol(symbol),
                    ))
                }),
        );

        // 3. Exports (only used by exe / dll objects)
        if let Ok(exports) = object_file.exports() {
            for export in exports {
                entries.push((
                    (export.address() - base_address) as u32,
                    FullSymbolListEntry::Export(export),
                ));
            }
        }

        // 4. Placeholder symbols based on function start addresses
        if let Some(function_start_addresses) = function_start_addresses {
            // Use function start addresses with synthesized symbols of the form fun_abcdef
            // as the ultimate fallback.
            // These synhesized symbols make it so that, for libraries which only contain symbols
            // for a small subset of their functions, we will show placeholder function names
            // rather than plain incorrect function names.
            entries.extend(
                function_start_addresses
                    .iter()
                    .map(|address| (*address, FullSymbolListEntry::Synthesized)),
            );
        }

        // 5. A placeholder symbol for the entry point.
        if let Some(entry_point) = object_file.entry().checked_sub(base_address) {
            entries.push((
                entry_point as u32,
                FullSymbolListEntry::SynthesizedEntryPoint,
            ));
        }

        // 6. End addresses from text section ends
        // These entries serve to "terminate" the last function of each section,
        // so that addresses in the following section are not considered
        // to be part of the last function of that previous section.
        entries.extend(
            object_file
                .sections()
                .filter(|s| s.kind() == SectionKind::Text)
                .filter_map(|section| {
                    let vma_end_address = section.address().checked_add(section.size())?;
                    let end_address = vma_end_address.checked_sub(base_address)?;
                    let end_address = u32::try_from(end_address).ok()?;
                    Some((end_address, FullSymbolListEntry::EndAddress))
                }),
        );

        // 7. End addresses for sized symbols
        // These addresses serve to "terminate" functions symbols.
        entries.extend(
            object_file
                .symbols()
                .filter(|symbol| {
                    symbol.kind() == SymbolKind::Text && symbol.address() != 0 && symbol.size() != 0
                })
                .filter_map(|symbol| {
                    Some((
                        u32::try_from(
                            symbol
                                .address()
                                .checked_add(symbol.size())?
                                .checked_sub(base_address)?,
                        )
                        .ok()?,
                        FullSymbolListEntry::EndAddress,
                    ))
                }),
        );

        // 8. End addresses for known functions ends
        // These addresses serve to "terminate" functions from function_start_addresses.
        // They come from .eh_frame or .pdata info, which has the function size.
        if let Some(function_end_addresses) = function_end_addresses {
            entries.extend(
                function_end_addresses
                    .iter()
                    .map(|address| (*address, FullSymbolListEntry::EndAddress)),
            );
        }

        // Done.
        // Now that all entries are added, sort and de-duplicate so that we only
        // have one entry per address.
        // If multiple entries for the same address are present, only the first
        // entry for that address is kept. (That's also why we use a stable sort
        // here.)
        // We have added entries in the order best to worst, so we keep the "best"
        // symbol for each address.
        entries.sort_by_key(|(address, _)| *address);
        entries.dedup_by_key(|(address, _)| *address);

        let path_mapper = Mutex::new(PathMapper::new());

        let mut svma_file_ranges: Vec<SvmaFileRange> = object_file
            .segments()
            .map(SvmaFileRange::from_segment)
            .collect();

        if svma_file_ranges.is_empty() {
            // If no segment is found, fall back to using section information.
            svma_file_ranges = object_file
                .sections()
                .filter_map(SvmaFileRange::from_section)
                .collect();
        }

        ObjectSymbolMapInnerImpl {
            entries,
            debug_id,
            path_mapper,
            object_map: object_file.object_map(),
            context: addr2line_context,
            image_base_address: base_address,
            svma_file_ranges,
        }
    }

    fn file_offset_to_svma(&self, offset: u64) -> Option<u64> {
        for svma_file_range in &self.svma_file_ranges {
            if svma_file_range.file_offset <= offset
                && offset < svma_file_range.file_offset + svma_file_range.size
            {
                let offset_from_range_start = offset - svma_file_range.file_offset;
                let svma = svma_file_range.svma.checked_add(offset_from_range_start)?;
                return Some(svma);
            }
        }
        None
    }
}

impl<'a, Symbol: object::ObjectSymbol<'a>> SymbolMapTrait for ObjectSymbolMapInnerImpl<'a, Symbol> {
    fn debug_id(&self) -> DebugId {
        self.debug_id
    }

    fn symbol_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|&(_, entry)| {
                matches!(
                    entry,
                    FullSymbolListEntry::Symbol(_) | FullSymbolListEntry::Export(_)
                )
            })
            .count()
    }

    fn iter_symbols(&self) -> Box<dyn Iterator<Item = (u32, Cow<'_, str>)> + '_> {
        Box::new(SymbolMapIter {
            inner: self.entries.iter(),
        })
    }

    fn lookup_relative_address(&self, address: u32) -> Option<AddressInfo> {
        let index = match self
            .entries
            .binary_search_by_key(&address, |&(addr, _)| addr)
        {
            Err(0) => return None,
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let (start_addr, entry) = &self.entries[index];
        let next_entry = self.entries.get(index + 1);
        // If the found entry is an EndAddress entry, this means that `address` falls
        // in the dead space between known functions, and we consider it to be not found.
        // In that case, entry.name returns Err().
        if let (Ok(name), Some((end_addr, _))) = (entry.name(*start_addr), next_entry) {
            let function_size = end_addr - *start_addr;

            let mut path_mapper = self.path_mapper.lock().unwrap();

            let svma = self.image_base_address + u64::from(address);
            let frames = match self.context.as_ref().map(|ctx| ctx.find_frames(svma)) {
                Some(LookupResult::Load { load, .. }) => {
                    let requested_dwo_ref = DwoRef::from_split_dwarf_load(&load);
                    FramesLookupResult::NeedDwo {
                        svma,
                        dwo_ref: requested_dwo_ref,
                        partial_frames: None,
                    }
                }
                Some(LookupResult::Output(Ok(frame_iter))) => {
                    if let Some(frames) = convert_frames(frame_iter, &mut path_mapper) {
                        FramesLookupResult::Available(frames)
                    } else {
                        FramesLookupResult::Unavailable
                    }
                }
                _ => {
                    if let Some(entry) = self.object_map.get(svma) {
                        let external_file_name = entry.object(&self.object_map);
                        let external_file_name = std::str::from_utf8(external_file_name).unwrap();
                        let offset_from_symbol = (svma - entry.address()) as u32;
                        let symbol_name = entry.name().to_owned();
                        let (file_name, address_in_file) = match external_file_name.find('(') {
                            Some(index) => {
                                // This is an "archive" reference of the form
                                // "/Users/mstange/code/obj-m-opt/toolkit/library/build/../../../js/src/build/libjs_static.a(Unified_cpp_js_src13.o)"
                                let (path, paren_rest) = external_file_name.split_at(index);
                                let name_in_archive = paren_rest
                                    .trim_start_matches('(')
                                    .trim_end_matches(')')
                                    .to_owned();
                                let address_in_file =
                                    ExternalFileAddressInFileRef::MachoOsoArchive {
                                        name_in_archive,
                                        symbol_name,
                                        offset_from_symbol,
                                    };
                                (path, address_in_file)
                            }
                            None => {
                                // This is a reference to a regular object file. Example:
                                // "/Users/mstange/code/obj-m-opt/toolkit/library/build/../../components/sessionstore/Unified_cpp_sessionstore0.o"
                                let address_in_file =
                                    ExternalFileAddressInFileRef::MachoOsoObject {
                                        symbol_name,
                                        offset_from_symbol,
                                    };
                                (external_file_name, address_in_file)
                            }
                        };
                        FramesLookupResult::External(ExternalFileAddressRef {
                            file_ref: ExternalFileRef {
                                file_name: file_name.to_owned(),
                            },
                            address_in_file,
                        })
                    } else {
                        FramesLookupResult::Unavailable
                    }
                }
            };

            let name = demangle::demangle_any(&name);
            Some(AddressInfo {
                symbol: SymbolInfo {
                    address: *start_addr,
                    size: Some(function_size),
                    name,
                },
                frames,
            })
        } else {
            None
        }
    }

    fn lookup_svma(&self, svma: u64) -> Option<AddressInfo> {
        let relative_address = svma.checked_sub(self.image_base_address)?.try_into().ok()?;
        // 4200608 2103456 2097152
        self.lookup_relative_address(relative_address)
    }

    fn lookup_offset(&self, offset: u64) -> Option<AddressInfo> {
        let svma = self.file_offset_to_svma(offset)?;
        self.lookup_svma(svma)
    }
}

pub struct SymbolMapIter<'data, 'map, Symbol: object::ObjectSymbol<'data>> {
    inner: slice::Iter<'map, (u32, FullSymbolListEntry<'data, Symbol>)>,
}

impl<'data, 'map, Symbol: object::ObjectSymbol<'data>> Iterator
    for SymbolMapIter<'data, 'map, Symbol>
{
    type Item = (u32, Cow<'map, str>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let &(address, ref symbol) = self.inner.next()?;
            let name = match symbol.name(address) {
                Ok(name) => name,
                Err(_) => continue,
            };
            return Some((address, name));
        }
    }
}

pub struct ObjectSymbolMapWithDwoSupportInnerImpl<
    'a,
    Symbol: object::ObjectSymbol<'a>,
    FC,
    ADAMD: AddDwoAndMakeDwarf<FC>,
> {
    regular_inner: ObjectSymbolMapInnerImpl<'a, Symbol>,
    adamd: &'a ADAMD,
    _phantom: PhantomData<FC>,
}

pub trait ObjectSymbolMapWithDwoSupportOuter<FC> {
    fn make_symbol_map_inner(&self) -> Result<ObjectSymbolMapWithDwoSupportInner<'_, FC>, Error>;
}

#[derive(Yokeable)]
pub struct ObjectSymbolMapWithDwoSupportInner<'data, FC>(
    pub Box<dyn SymbolMapTraitWithLookupFramesExt<FC> + Send + 'data>,
);

pub struct ObjectSymbolMapWithDwoSupport<
    FC: 'static,
    OSMWDSO: ObjectSymbolMapWithDwoSupportOuter<FC>,
>(Yoke<ObjectSymbolMapWithDwoSupportInner<'static, FC>, Box<OSMWDSO>>);

impl<FC, OSMWDSO: ObjectSymbolMapWithDwoSupportOuter<FC> + 'static>
    ObjectSymbolMapWithDwoSupport<FC, OSMWDSO>
{
    pub fn new(outer: OSMWDSO) -> Result<Self, Error> {
        let outer_and_inner =
            Yoke::<ObjectSymbolMapWithDwoSupportInner<FC>, _>::try_attach_to_cart(
                Box::new(outer),
                |outer| outer.make_symbol_map_inner(),
            )?;
        Ok(ObjectSymbolMapWithDwoSupport(outer_and_inner))
    }
}

impl<FC: FileContents + 'static, OSMWDSO: ObjectSymbolMapWithDwoSupportOuter<FC>>
    GetInnerSymbolMapWithLookupFramesExt<FC> for ObjectSymbolMapWithDwoSupport<FC, OSMWDSO>
{
    fn get_inner_symbol_map<'a>(&'a self) -> &'a (dyn SymbolMapTraitWithLookupFramesExt<FC> + 'a) {
        self.0.get().0.as_ref()
    }
}

impl<'a, Symbol: object::ObjectSymbol<'a>, FC, ADAMD: AddDwoAndMakeDwarf<FC>>
    ObjectSymbolMapWithDwoSupportInnerImpl<'a, Symbol, FC, ADAMD>
{
    pub fn new<'file, O>(
        object_file: &'file O,
        addr2line_context: Option<addr2line::Context<EndianSlice<'a, RunTimeEndian>>>,
        debug_id: DebugId,
        function_start_addresses: Option<&[u32]>,
        function_end_addresses: Option<&[u32]>,
        adamd: &'a ADAMD,
    ) -> Self
    where
        'a: 'file,
        O: object::Object<'a, 'file, Symbol = Symbol>,
        Symbol: object::ObjectSymbol<'a> + Send + 'a,
    {
        let regular_inner = ObjectSymbolMapInnerImpl::new(
            object_file,
            addr2line_context,
            debug_id,
            function_start_addresses,
            function_end_addresses,
        );
        Self {
            regular_inner,
            adamd,
            _phantom: PhantomData,
        }
    }

    fn convert_lookup_result<
        's,
        ALC: addr2line::LookupContinuation<
                Buf = EndianSlice<'a, RunTimeEndian>,
                Output = Result<
                    addr2line::FrameIter<'s, EndianSlice<'a, RunTimeEndian>>,
                    addr2line::gimli::Error,
                >,
            > + 's,
    >(
        &'s self,
        lookup_result: addr2line::LookupResult<ALC>,
    ) -> FramesLookupResult2
    where
        'a: 's,
    {
        match lookup_result {
            LookupResult::Load { load, .. } => {
                let requested_dwo_ref = DwoRef::from_split_dwarf_load(&load);
                FramesLookupResult2::NeedDwo(requested_dwo_ref)
            }
            LookupResult::Output(Ok(frame_iter)) => {
                let mut path_mapper = self.regular_inner.path_mapper.lock().unwrap();
                FramesLookupResult2::Done(convert_frames(frame_iter, &mut path_mapper))
            }
            LookupResult::Output(Err(_err)) => FramesLookupResult2::Done(None),
        }
    }
}

impl<'a, Symbol: object::ObjectSymbol<'a>, FC, ADAMD: AddDwoAndMakeDwarf<FC>> SymbolMapTrait
    for ObjectSymbolMapWithDwoSupportInnerImpl<'a, Symbol, FC, ADAMD>
{
    fn debug_id(&self) -> DebugId {
        self.regular_inner.debug_id()
    }

    fn symbol_count(&self) -> usize {
        self.regular_inner.symbol_count()
    }

    fn iter_symbols(&self) -> Box<dyn Iterator<Item = (u32, Cow<'_, str>)> + '_> {
        self.regular_inner.iter_symbols()
    }

    fn lookup_relative_address(&self, address: u32) -> Option<AddressInfo> {
        self.regular_inner.lookup_relative_address(address)
    }

    fn lookup_svma(&self, svma: u64) -> Option<AddressInfo> {
        self.regular_inner.lookup_svma(svma)
    }

    fn lookup_offset(&self, offset: u64) -> Option<AddressInfo> {
        self.regular_inner.lookup_offset(offset)
    }
}

pub trait AddDwoAndMakeDwarf<FC> {
    fn add_dwo_and_make_dwarf(
        &self,
        file_contents: FC,
    ) -> Result<
        addr2line::gimli::Dwarf<addr2line::gimli::EndianSlice<'_, addr2line::gimli::RunTimeEndian>>,
        Error,
    >;
}

impl<
        'a,
        Symbol: object::ObjectSymbol<'a>,
        FC: FileContents + 'static,
        ADAMD: AddDwoAndMakeDwarf<FC>,
    > SymbolMapTraitWithLookupFramesExt<FC>
    for ObjectSymbolMapWithDwoSupportInnerImpl<'a, Symbol, FC, ADAMD>
{
    fn get_as_symbol_map(&self) -> &dyn SymbolMapTrait {
        self
    }

    fn lookup_frames_again(&self, svma: u64) -> FramesLookupResult2 {
        let Some(ctx) = self.regular_inner.context.as_ref() else {
            return FramesLookupResult2::Done(None);
        };
        let lookup_result = ctx.find_frames(svma);
        self.convert_lookup_result(lookup_result)
    }

    fn lookup_frames_more(
        &self,
        svma: u64,
        dwo_ref: &DwoRef,
        file_contents: Option<FC>,
    ) -> FramesLookupResult2 {
        let Some(ctx) = self.regular_inner.context.as_ref() else {
            return FramesLookupResult2::Done(None);
        };
        let lookup_result = ctx.find_frames(svma);
        match lookup_result {
            LookupResult::Load { load, continuation } => {
                let requested_dwo_ref = DwoRef::from_split_dwarf_load(&load);
                if &requested_dwo_ref == dwo_ref {
                    let maybe_dwarf = file_contents
                        .and_then(|file_contents| {
                            self.adamd.add_dwo_and_make_dwarf(file_contents).ok()
                        })
                        .map(|mut dwo_dwarf| {
                            dwo_dwarf.make_dwo(&*load.parent);
                            Arc::new(dwo_dwarf)
                        });
                    use addr2line::LookupContinuation;
                    let lookup_result = continuation.resume(maybe_dwarf);
                    self.convert_lookup_result(lookup_result)
                } else {
                    FramesLookupResult2::NeedDwo(requested_dwo_ref)
                }
            }
            LookupResult::Output(Ok(frame_iter)) => {
                let mut path_mapper = self.regular_inner.path_mapper.lock().unwrap();
                FramesLookupResult2::Done(convert_frames(frame_iter, &mut path_mapper))
            }
            LookupResult::Output(Err(_err)) => FramesLookupResult2::Done(None),
        }
    }
}

impl DwoRef {
    fn from_split_dwarf_load(load: &SplitDwarfLoad<EndianSlice<RunTimeEndian>>) -> Self {
        let comp_dir = String::from_utf8_lossy(load.comp_dir.unwrap().slice()).to_string();
        let path = String::from_utf8_lossy(load.path.unwrap().slice()).to_string();
        let dwo_id = load.dwo_id.0;
        Self {
            comp_dir,
            path,
            dwo_id,
        }
    }
}
