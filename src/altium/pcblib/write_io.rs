//! `PcbLib` write/serialisation path: the `impl PcbLib` methods (incl. the
//! public `write` entry) that serialise a library to an OLE compound document.
//! Split out of `mod.rs` for navigability; same `impl PcbLib`.

use super::{
    writer, AltiumError, AltiumResult, ComponentBody, EmbeddedModel, Footprint, Layer, Model3D,
    PcbLib,
};

impl PcbLib {
    /// Writes the library to any writer implementing `Read + Write + Seek`.
    ///
    /// Takes `&mut self` because it materialises referenced 3D models
    /// (`prepare_3d_models_for_writing`) before serialising.
    ///
    /// # Errors
    ///
    /// Returns an error if the library cannot be serialised.
    pub fn write(
        &mut self,
        writer: impl std::io::Read + std::io::Write + std::io::Seek,
    ) -> AltiumResult<()> {
        // A library read from disk is written back in the code page it was
        // authored in; one built in memory, in the server's.
        let encoding = self.ansi_encoding();
        crate::altium::with_ansi_encoding(encoding, || self.write_scoped(writer))
    }

    /// The body of [`Self::write`], run under the library's ANSI encoding.
    fn write_scoped(
        &mut self,
        writer: impl std::io::Read + std::io::Write + std::io::Seek,
    ) -> AltiumResult<()> {
        // An empty name has no storage name to derive — the root storage
        // would be "created" twice and the save fail half-way — so refuse it
        // before touching the file.
        if let Some(i) = self.footprints.iter().position(|f| f.name.is_empty()) {
            return Err(AltiumError::InvalidParameter {
                name: "name".to_string(),
                message: format!("footprint {i} has an empty name"),
            });
        }
        for footprint in &self.footprints {
            footprint
                .check_record_text()
                .map_err(|message| AltiumError::InvalidParameter {
                    name: "text".to_string(),
                    message,
                })?;
        }

        // Convert model_3d references to ComponentBody + EmbeddedModel before writing
        self.prepare_3d_models_for_writing()?;

        let mut cfb = crate::altium::create_ole(writer)?;

        // Every footprint's PATTERN text — the bytes Library/Data, the Data
        // stream's name block and SectionKeys carry as well: the bytes Altium
        // wrote while they still describe the name, else the name's wire form.
        let wire_names: Vec<String> = self
            .footprints
            .iter()
            .map(writer::footprint_pattern_text)
            .collect();
        // Each footprint keeps the storage it was read from; one built from
        // scratch, renamed or copied gets a name derived by Altium's rule from
        // the name Altium will read: kept as it is when it fits the cap
        // (`ᏣᎳᎩ_CR_0402` is stored under `ᏣᎳᎩ_CR_0402`), else the PATTERN
        // bytes cut at the cap and read back through the code page, as Altium
        // derives it, and mapped back through SectionKeys.
        let altium_names: Vec<String> = self
            .footprints
            .iter()
            .map(writer::footprint_name_as_altium_reads_it)
            .collect();
        let ole_names = crate::altium::resolve_storage_names(
            &wire_names
                .iter()
                .zip(altium_names.iter())
                .zip(self.footprints.iter())
                .map(|((wire, altium), fp)| {
                    let seed =
                        if crate::altium::utf16_len(altium) <= crate::altium::MAX_OLE_NAME_LEN {
                            altium.clone()
                        } else {
                            crate::altium::ansi_cut_storage_name(
                                wire,
                                crate::altium::current_ansi_encoding(),
                            )
                        };
                    (seed, fp.storage_name.clone())
                })
                .collect::<Vec<_>>(),
        );

        // Write FileHeader (pipe-delimited format for reader compatibility)
        self.write_file_header(&mut cfb)?;

        // Write Library storage (Header + Data for Altium compatibility). Its
        // component list carries every footprint's real (wire) name, as
        // Altium writes it: the storage name is derived from it on read, or
        // mapped through SectionKeys when the name reaches the cap.
        self.write_library(&mut cfb, &wire_names)?;

        // Root SectionKeys stream, in the binary layout Altium reads (#507):
        // the LibRef -> storage-name map for every footprint whose name
        // reaches the 31-unit storage cap — truncated, or merely filling it
        // (an AD21-authored library lists a 31-character name as an identity
        // pair). A shorter name is not listed even when its storage differs:
        // Altium re-derives that storage from the name itself. The real name
        // still travels in the footprint's own PATTERN parameter and in
        // Library/Data. Not written when no name reaches the cap — which
        // includes the whole golden.
        let truncated: Vec<(String, String)> = wire_names
            .iter()
            .zip(altium_names.iter())
            .zip(ole_names.iter())
            .filter(|((_, altium), _)| {
                crate::altium::utf16_len(altium) >= crate::altium::MAX_OLE_NAME_LEN
            })
            .map(|((wire, _), ole)| (wire.clone(), crate::altium::section_key_name(wire, ole)))
            .collect();
        // Altium lists the entries in alphabetical order of the name (every
        // Altium-written SectionKeys in the corpus: six libraries, all
        // upper-case names, so the case rule is untested — case-insensitive
        // here, as Delphi's string lists sort, with the bytes as tie-break).
        let mut truncated = truncated;
        truncated.sort_by(|(a, _), (b, _)| {
            a.to_ascii_uppercase()
                .cmp(&b.to_ascii_uppercase())
                .then_with(|| a.cmp(b))
        });
        if let Some(section_keys) = crate::altium::encode_pcblib_section_keys(&truncated)? {
            crate::altium::write_stream(&mut cfb, "/SectionKeys", &section_keys)?;
        }

        // Write embedded 3D models if present (under /Library/Models/)
        self.write_models(&mut cfb)?;

        // Write each footprint using its OLE-safe name
        for (footprint, ole_name) in self.footprints.iter().zip(ole_names.iter()) {
            self.write_footprint(&mut cfb, footprint, ole_name)?;
        }

        // Write the root FileVersionInfo metadata storage: as read, else
        // Altium's own.
        if !self.write_carried_storage(&mut cfb, "/FileVersionInfo")? {
            Self::write_file_version_info(&mut cfb)?;
        }

        tracing::info!(
            count = self.footprints.len(),
            models = self.models.len(),
            "Wrote PcbLib"
        );

        Ok(())
    }

    /// Converts `model_3d` references to `ComponentBody` + `EmbeddedModel` for writing.
    ///
    /// This method processes all footprints that have a `model_3d` field set:
    /// 1. Reads the STEP file from disk (using the filepath)
    /// 2. Creates an `EmbeddedModel` with a generated GUID
    /// 3. Creates a `ComponentBody` referencing the model
    /// 4. Adds the `ComponentBody` to the footprint's `component_bodies`
    /// 5. Adds the `EmbeddedModel` to the library's `models` collection
    ///
    /// # Errors
    ///
    /// Returns an error if a STEP file cannot be read.
    #[allow(clippy::too_many_lines)] // one decision per model_3d case, kept together
    fn prepare_3d_models_for_writing(&mut self) -> AltiumResult<()> {
        use uuid::Uuid;

        for footprint in &mut self.footprints {
            let Some(ref model_3d) = footprint.model_3d else {
                continue;
            };
            let path = std::path::Path::new(&model_3d.filepath);

            // Only ever surface the bare file name in logs/errors, never the
            // caller's full path (sanitisation rule).
            let display_name = path.file_name().map_or_else(
                || "<model>".to_string(),
                |n| n.to_string_lossy().into_owned(),
            );

            // The three conditions that decide what to do with this model_3d.
            let has_bodies = !footprint.component_bodies.is_empty();
            // An explicit path has directory components; a bare model name (set
            // when reading a ComponentBody back) does not.
            let is_explicit_path = path.parent().is_some_and(|p| !p.as_os_str().is_empty());
            let file_present = path.exists() && path.is_file();

            if file_present
                && Self::embed_into_supplied_body(
                    path,
                    model_3d,
                    &mut footprint.component_bodies,
                    &mut self.models,
                )?
            {
                tracing::debug!(
                    footprint = %footprint.name,
                    model = %display_name,
                    "Embedded model_3d into the caller's ComponentBody"
                );
                continue;
            }

            match (has_bodies, is_explicit_path, file_present) {
                // New footprint whose STEP file is missing — the path is required.
                (false, _, false) => {
                    return Err(AltiumError::InvalidParameter {
                        name: "step_model.filepath".to_string(),
                        message: format!(
                            "STEP file not found for footprint '{}': '{}'. \
                             Provide a valid path or use embed: false for external reference.",
                            footprint.name, display_name
                        ),
                    });
                }
                // Already embedded, and either the filepath is a bare model name
                // (from a prior read) or it does not point at a file — keep the
                // existing ComponentBody as-is.
                (true, false, _) | (true, _, false) => {
                    tracing::trace!(
                        footprint = %footprint.name,
                        model = %display_name,
                        "Skipping model_3d - already embedded or filepath not actionable"
                    );
                    continue;
                }
                // Already embedded but the user pointed at a new explicit path that
                // exists — re-embed. Drop the old ComponentBodies AND the models
                // they referenced, so the latter don't linger in self.models as
                // orphans, which would bloat the library on every save.
                (true, true, true) => {
                    Self::drop_stale_bodies(
                        &footprint.name.clone(),
                        &mut footprint.component_bodies,
                        &mut self.models,
                    );
                }
                // Fresh embed: new footprint, file present.
                (false, _, true) => {}
            }

            // Embed: read the STEP file, then create the EmbeddedModel +
            // ComponentBody (shared by the fresh-embed and re-embed cases above).
            let step_data = std::fs::read(path).map_err(|e| AltiumError::file_read(path, e))?;
            let guid = format!("{{{}}}", Uuid::new_v4().to_string().to_uppercase());
            let filename = path.file_name().map_or_else(
                || "model.step".to_string(),
                |n| n.to_string_lossy().to_string(),
            );

            self.models
                .push(EmbeddedModel::new(&guid, &filename, step_data));
            footprint.component_bodies.push(ComponentBody {
                model_id: guid,
                identifier: String::new(),
                texture_center_x: None,
                texture_center_y: None,
                texture_size_x: None,
                texture_size_y: None,
                texture_rotation: None,
                raw_layer_id: None,
                v7_layer: None,
                model_name: filename,
                embedded: true,
                rotation_x: 0.0,
                rotation_y: 0.0,
                rotation_z: model_3d.rotation,
                z_offset: model_3d.z_offset,
                overall_height: 0.0, // Could be calculated from STEP, but not implemented
                standoff_height: 0.0,
                cavity_height: 0.0,
                layer: Layer::Top3DBody,
                outline: Vec::new(), // Synthesised from the footprint extent on write
                unique_id: None,
                guid: None,
                model_checksum: 0, // Fresh embed; Altium computes the real checksum on save.
                name: " ".to_string(),
                kind: 0,
                sub_poly_index: -1,
                union_index: 0,
                is_shape_based: false,
                body_projection: 0,
                body_color_3d: 8_421_504,
                body_opacity_3d: 1.0,
                model_2d_rotation: 0.0,
                model_2d_x: 0.0,
                model_2d_y: 0.0,
                net_index: 0xFFFF,
                polygon_index: 0xFFFF,
                component_index: -1,
                additional_parameters: Vec::new(),
                param_key_order: Vec::new(),
            });

            tracing::debug!(
                footprint = %footprint.name,
                filepath = %model_3d.filepath,
                "Converted model_3d to ComponentBody"
            );
        }

        Ok(())
    }

    /// Generates OLE-safe names for all footprints.
    ///
    /// OLE Compound File names are limited to 31 characters. This method:
    /// - Returns names as-is if they fit within the limit
    /// - Truncates longer names and adds unique suffixes to avoid collisions
    ///
    /// The full footprint name is still stored in the PATTERN field.
    /// Embeds the STEP file at `path` into a body the caller supplied for it —
    /// one naming the file but referencing no model the library holds yet —
    /// and reports whether there was one. Its layer, heights, outline and
    /// placement are what the caller asked for, so it is bound to the new model
    /// rather than replaced by a default body.
    ///
    /// # Errors
    ///
    /// Returns an error if the STEP file cannot be read.
    fn embed_into_supplied_body(
        path: &std::path::Path,
        model_3d: &Model3D,
        bodies: &mut [ComponentBody],
        models: &mut Vec<EmbeddedModel>,
    ) -> AltiumResult<bool> {
        let filename = path.file_name().map_or_else(
            || "model.step".to_string(),
            |n| n.to_string_lossy().to_string(),
        );
        let Some(body) = bodies.iter_mut().find(|cb| {
            cb.model_name.eq_ignore_ascii_case(&filename)
                && !models
                    .iter()
                    .any(|m| m.id.eq_ignore_ascii_case(&cb.model_id))
        }) else {
            return Ok(false);
        };
        let step_data = std::fs::read(path).map_err(|e| AltiumError::file_read(path, e))?;
        let guid = format!("{{{}}}", uuid::Uuid::new_v4().to_string().to_uppercase());
        models.push(EmbeddedModel::new(&guid, &filename, step_data));
        body.model_id = guid;
        body.model_name = filename;
        body.embedded = true;
        body.model_checksum = 0; // Fresh embed; Altium computes it on save.
        if body.rotation_z == 0.0 {
            body.rotation_z = model_3d.rotation;
        }
        if body.z_offset == 0.0 {
            body.z_offset = model_3d.z_offset;
        }
        Ok(true)
    }

    /// Drops a footprint's embedded bodies and the models they referenced,
    /// ahead of a re-embed from a new explicit path — otherwise the old models
    /// linger in `self.models` as orphans and bloat the library on every save.
    fn drop_stale_bodies(
        name: &str,
        bodies: &mut Vec<ComponentBody>,
        models: &mut Vec<EmbeddedModel>,
    ) {
        tracing::debug!(
            footprint = %name,
            old_bodies = bodies.len(),
            "Re-embedding model_3d from new explicit path"
        );
        let stale: std::collections::HashSet<String> = bodies
            .iter()
            .filter(|cb| cb.embedded)
            .map(|cb| cb.model_id.to_lowercase())
            .collect();
        bodies.clear();
        models.retain(|m| !stale.contains(&m.id.to_lowercase()));
    }

    /// Writes embedded 3D models to `/Library/Models/` storage.
    ///
    /// Creates:
    /// - `/Library/Models/Header` - Model count and metadata
    /// - `/Library/Models/Data` - GUID-to-index mapping
    /// - `/Library/Models/{N}` - zlib-compressed STEP files
    fn write_models<F: std::io::Read + std::io::Write + std::io::Seek>(
        &self,
        cfb: &mut cfb::CompoundFile<F>,
    ) -> AltiumResult<()> {
        if self.models.is_empty() {
            return Ok(());
        }

        // Create /Library storage if it doesn't exist
        if !cfb.exists("/Library") {
            crate::altium::create_storage(cfb, "/Library")?;
        }

        // Create /Library/Models storage
        crate::altium::create_storage(cfb, "/Library/Models")?;

        // Write Header stream
        let header_data = writer::encode_model_header_stream(self.models.len());
        crate::altium::write_stream(cfb, "/Library/Models/Header", &header_data)?;

        // Write Data stream (GUID-to-index mapping)
        let data_content = writer::encode_model_data_stream(&self.models, &self.footprints);
        crate::altium::write_stream(cfb, "/Library/Models/Data", &data_content)?;

        // Write individual model streams (compressed)
        let compressed_models = writer::prepare_models_for_writing(&self.models)?;
        for (idx, compressed) in compressed_models {
            crate::altium::write_stream(cfb, &format!("/Library/Models/{idx}"), &compressed)?;
        }

        tracing::debug!(count = self.models.len(), "Wrote embedded 3D models");
        Ok(())
    }

    /// Writes the `/FileHeader` stream.
    ///
    /// The canonical `PcbLib` `FileHeader` is **53 bytes** with THREE fields
    /// (matching `AltiumSharp`'s `PcbLibWriter.WriteFileHeader`); Altium rejects
    /// the file if the version double or the `UniqueId` block are missing:
    /// ```text
    /// [len:4 LE u32][len:1 u8]["PCB 6.0 Binary Library File"]  // version string (4+1+27 = 32 bytes)
    /// [5.01 : 8 f64]                                            // format version double
    /// [len:4 LE u32][len:1 u8][8-char UniqueId]                // 4+1+8 = 13 bytes
    /// ```
    /// The version string's 4-byte and 1-byte lengths are the same value (27).
    /// Component metadata is stored in `/Library/Data`, not here.
    #[allow(clippy::unused_self)]
    fn write_file_header<F: std::io::Read + std::io::Write + std::io::Seek>(
        &self,
        cfb: &mut cfb::CompoundFile<F>,
    ) -> AltiumResult<()> {
        // The canonical PcbLib FileHeader is 53 bytes with THREE fields (matching
        // AltiumSharp PcbLibWriter.WriteFileHeader). Altium Designer rejects the
        // file if the 5.01 version double and the UniqueId block are missing.
        let version_string = b"PCB 6.0 Binary Library File";
        #[allow(clippy::cast_possible_truncation)]
        let len = version_string.len() as u32;

        // The library keeps the UniqueId it was read with; one built from
        // scratch is given its first here.
        let unique_id = self
            .metadata
            .unique_id
            .clone()
            .unwrap_or_else(crate::util::generate_unique_id);
        let uid_bytes = unique_id.as_bytes();
        #[allow(clippy::cast_possible_truncation)]
        let uid_len = uid_bytes.len() as u32; // always 8

        let mut data =
            Vec::with_capacity(4 + 1 + version_string.len() + 8 + 4 + 1 + uid_bytes.len());
        // Field 1: version string block ([u32 len][u8 len][bytes]).
        data.extend_from_slice(&len.to_le_bytes());
        #[allow(clippy::cast_possible_truncation)]
        data.push(len as u8);
        data.extend_from_slice(version_string);
        // Field 2: version double 5.01 (8 raw little-endian bytes, NO length prefix).
        data.extend_from_slice(&5.01_f64.to_le_bytes());
        // Field 3: 8-char UniqueId string block ([u32 len][u8 len][bytes]).
        data.extend_from_slice(&uid_len.to_le_bytes());
        #[allow(clippy::cast_possible_truncation)]
        data.push(uid_len as u8);
        data.extend_from_slice(uid_bytes);

        crate::altium::write_stream(cfb, "/FileHeader", &data)?;

        Ok(())
    }

    /// Writes the `/Library` storage with Header and Data streams.
    ///
    /// # Streams Created
    ///
    /// - `/Library/Header` - 4-byte record count (always 1)
    /// - `/Library/Data` - Library parameters + component count + component names
    ///
    /// # Format
    ///
    /// Library/Data:
    /// ```text
    /// [block_len:4]["|KEY=VAL|..." + \x00]   // parameter block (null-terminated)
    /// [component_count:4 LE u32]
    /// [block_len:4][str_len:1][name]          // per component (WriteStringBlock)
    /// ```
    fn write_library<F: std::io::Read + std::io::Write + std::io::Seek>(
        &self,
        cfb: &mut cfb::CompoundFile<F>,
        ole_names: &[String],
    ) -> AltiumResult<()> {
        // Create /Library storage
        crate::altium::create_storage(cfb, "/Library")?;

        // Write Library/Header (record count = 1)
        crate::altium::write_stream(cfb, "/Library/Header", &1u32.to_le_bytes())?;

        // Build Library/Data content: a C-string parameter block, then the
        // component count + names.
        let params = self.library_params();
        let mut data = Vec::new();
        crate::altium::framing::write_cstring_param_block(&mut data, &params);

        // Component count
        #[allow(clippy::cast_possible_truncation)]
        data.extend_from_slice(&(self.footprints.len() as u32).to_le_bytes());

        // Component names as WriteStringBlock: [block_len:4][str_len:1][string].
        // The read-side mirror is `read_library_data`.
        for ole_name in ole_names {
            crate::altium::framing::write_string_block(
                &mut data,
                &crate::altium::encode_windows1252(ole_name),
            );
        }

        // Write Library/Data
        crate::altium::write_stream(cfb, "/Library/Data", &data)?;

        // Write the library metadata storages Altium emits for every library.
        self.write_library_metadata(cfb)?;

        Ok(())
    }

    /// Wraps a parameter string as an Altium C-string block:
    /// `[block_len:4][text + \x00]` where `block_len` includes the terminator.
    ///
    /// Thin owned-`Vec` wrapper around the shared
    /// [`crate::altium::framing::write_cstring_param_block`] frame.
    fn param_block(text: &str) -> Vec<u8> {
        let mut v = Vec::new();
        crate::altium::framing::write_cstring_param_block(
            &mut v,
            &crate::altium::encode_windows1252(text),
        );
        v
    }

    /// Writes the storage at `path` back exactly as it was read (see
    /// [`super::LibraryMetadata::carried_storages`]), sub-storages included;
    /// `false`, writing nothing, when the library carries no stream under it.
    fn write_carried_storage<F: std::io::Read + std::io::Write + std::io::Seek>(
        &self,
        cfb: &mut cfb::CompoundFile<F>,
        path: &str,
    ) -> AltiumResult<bool> {
        let prefix = format!("{path}/");
        let streams: Vec<&(String, Vec<u8>)> = self
            .metadata
            .carried_storages
            .iter()
            .filter(|(stream, _)| stream.starts_with(&prefix))
            .collect();
        if streams.is_empty() {
            return Ok(false);
        }
        crate::altium::create_storage(cfb, path)?;
        for (stream, bytes) in streams {
            // Any sub-storage between the storage and the stream.
            let mut storage = path.to_string();
            let inner: Vec<&str> = stream[prefix.len()..].split('/').collect();
            for name in &inner[..inner.len() - 1] {
                storage = format!("{storage}/{name}");
                if !cfb.exists(&storage) {
                    crate::altium::create_storage(cfb, &storage)?;
                }
            }
            crate::altium::write_stream(cfb, stream, bytes)?;
        }
        Ok(true)
    }

    /// Creates a child storage containing a `Header` (record count) stream and a
    /// `Data` stream — the shape every Altium metadata storage uses.
    fn write_meta_storage<F: std::io::Read + std::io::Write + std::io::Seek>(
        cfb: &mut cfb::CompoundFile<F>,
        path: &str,
        header_count: u32,
        data: &[u8],
    ) -> AltiumResult<()> {
        crate::altium::create_storage(cfb, path)?;
        crate::altium::write_stream(cfb, &format!("{path}/Header"), &header_count.to_le_bytes())?;
        crate::altium::write_stream(cfb, &format!("{path}/Data"), data)?;
        Ok(())
    }

    /// The `/Library/ComponentParamsTOC/Data` stream this crate builds: one
    /// CRLF-terminated line per footprint, in the library's ANSI code page.
    ///
    /// A footprint unchanged since it was read keeps the line Altium wrote for
    /// it (its Unicode twins and code page included); any other gets a line
    /// built here, its height in mils as Altium prints it (`39.3701`).
    pub(crate) fn component_params_toc_stream(&self) -> Vec<u8> {
        let read_lines = self.read_toc_lines();
        let mut toc = Vec::new();
        for fp in &self.footprints {
            let kept = fp.unchanged_storage().and_then(|_| {
                read_lines.get(&crate::altium::encode_ansi(&fp.name, self.ansi_encoding()))
            });
            if let Some(line) = kept {
                toc.extend_from_slice(line);
            } else {
                let line = format!(
                    "Name={}|Pad Count={}|Height={}|Description={}",
                    fp.name,
                    fp.pads.len(),
                    toc_height_mils(fp.height),
                    fp.description
                );
                toc.extend_from_slice(&crate::altium::encode_ansi(&line, self.ansi_encoding()));
            }
            toc.extend_from_slice(b"\r\n");
        }
        let mut stream = Vec::new();
        crate::altium::framing::write_cstring_param_block(&mut stream, &toc);
        stream
    }

    /// The lines of the `ComponentParamsTOC` read with the library, by the
    /// `Name=` bytes each starts with.
    fn read_toc_lines(&self) -> std::collections::HashMap<Vec<u8>, Vec<u8>> {
        let Some((read, _)) = &self.metadata.component_params_toc else {
            return std::collections::HashMap::new();
        };
        let body = read.get(4..).unwrap_or_default();
        let body = body.strip_suffix(b"\0").unwrap_or(body);
        body.split(|&b| b == b'\n')
            .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
            .filter_map(|line| {
                let name = line.strip_prefix(b"Name=")?;
                let end = name.iter().position(|&b| b == b'|').unwrap_or(name.len());
                Some((name[..end].to_vec(), line.to_vec()))
            })
            .collect()
    }

    /// Writes the `/Library` metadata storages that Altium emits for every
    /// library (`LayerKindMapping`, `PadViaLibrary`, `ComponentParamsTOC`,
    /// `Textures` and `ModelsNoEmbed`, the last two empty unless the library
    /// read carried them). Without these, Altium Designer considers the
    /// library incomplete.
    fn write_library_metadata<F: std::io::Read + std::io::Write + std::io::Seek>(
        &self,
        cfb: &mut cfb::CompoundFile<F>,
    ) -> AltiumResult<()> {
        use uuid::Uuid;

        // LayerKindMapping: the library's own, as read; else an empty mapping,
        // [u32 textLen][UTF-16LE "1.0\0"][u32 signature=0][u32 count=0].
        let lkm = self.metadata.layer_kind_mapping.clone().unwrap_or_else(|| {
            let text16: Vec<u8> = "1.0\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
            let mut lkm = Vec::with_capacity(text16.len() + 12);
            #[allow(clippy::cast_possible_truncation)]
            lkm.extend_from_slice(&(text16.len() as u32).to_le_bytes());
            lkm.extend_from_slice(&text16);
            lkm.extend_from_slice(&0u32.to_le_bytes()); // signature
            lkm.extend_from_slice(&0u32.to_le_bytes()); // entry count
            lkm
        });
        Self::write_meta_storage(cfb, "/Library/LayerKindMapping", 1, &lkm)?;

        // PadViaLibrary: the library's own templates, as read; else an empty
        // cache under the library id it was read with (a fresh one for a
        // library built from scratch).
        if !self.write_carried_storage(cfb, "/Library/PadViaLibrary")? {
            let library_id =
                self.metadata.pad_via_library_id.clone().unwrap_or_else(|| {
                    format!("{{{}}}", Uuid::new_v4().to_string().to_uppercase())
                });
            let pvl = Self::param_block(&format!(
                "|PADVIALIBRARY.LIBRARYID={library_id}|PADVIALIBRARY.LIBRARYNAME=<Local>|PADVIALIBRARY.DISPLAYUNITS=1"
            ));
            Self::write_meta_storage(cfb, "/Library/PadViaLibrary", 0, &pvl)?;
        }

        // ComponentParamsTOC: Altium's own table while the footprints still
        // build the one it was read with; else this crate's.
        let built = self.component_params_toc_stream();
        let toc = match &self.metadata.component_params_toc {
            Some((read, basis)) if *basis == built => read.clone(),
            _ => built,
        };
        Self::write_meta_storage(cfb, "/Library/ComponentParamsTOC", 1, &toc)?;

        // Textures and ModelsNoEmbed: as read; else empty.
        for path in ["/Library/Textures", "/Library/ModelsNoEmbed"] {
            if !self.write_carried_storage(cfb, path)? {
                Self::write_meta_storage(cfb, path, 0, &[])?;
            }
        }

        // EmbeddedFonts: the library's own fonts, as read; else a plain u32
        // font count of 0.
        let fonts = self
            .metadata
            .embedded_fonts
            .clone()
            .unwrap_or_else(|| 0u32.to_le_bytes().to_vec());
        crate::altium::write_stream(cfb, "/Library/EmbeddedFonts", &fonts)?;

        // Empty Models storage when the library has no embedded models
        // (otherwise write_models creates it). Altium expects it to exist.
        if self.models.is_empty() {
            Self::write_meta_storage(cfb, "/Library/Models", 0, &[])?;
        }

        Ok(())
    }

    /// The `/FileVersionInfo` payload — Altium's version-history metadata, stored
    /// as reviewable `|KEY=VALUE|` text rather than an opaque binary blob, so any
    /// change shows up in a diff (it cannot be silently swapped). The C-string
    /// block framing (`[u32 len][text][0x00]`) is re-added on write, reproducing
    /// byte-for-byte the stream Altium emits.
    pub(crate) const FVI_TEXT: &str = include_str!("assets/file_version_info.txt");

    /// Writes the root `/FileVersionInfo` storage of a library that carries
    /// none of its own: a fixed, library-agnostic version-history blob (see
    /// [`Self::FVI_TEXT`]).
    fn write_file_version_info<F: std::io::Read + std::io::Write + std::io::Seek>(
        cfb: &mut cfb::CompoundFile<F>,
    ) -> AltiumResult<()> {
        let mut data = Vec::new();
        crate::altium::framing::write_cstring_param_block(&mut data, Self::FVI_TEXT.as_bytes());
        Self::write_meta_storage(cfb, "/FileVersionInfo", 1, &data)
    }

    /// The encoded parameter block for `/Library/Data`.
    ///
    /// Format: `|KEY=VAL|KEY=VAL|...` (leading pipe, NO trailing pipe).
    ///
    /// A library read from a file replays its own block, so the stack a
    /// designer configured — custom mechanical layer names, which layers are
    /// enabled, the layer sets, the view state — comes back out unchanged.
    /// Only the three keys that describe *this* save are rewritten. A library
    /// built in memory has no stack to preserve and gets the template one.
    fn library_params(&self) -> Vec<u8> {
        // A library path is as free to leave Windows-1252 as any other string
        // Altium stores, so it goes on the wire the same way.
        let filename = crate::altium::to_wire_text(self.filepath.as_deref().unwrap_or(""));
        let now = chrono::Local::now();
        let (date, time) = (
            now.format("%d. %m. %Y").to_string(),
            now.format("%H:%M:%S").to_string(),
        );

        if let Some(stored) = self.metadata.library_params.as_deref() {
            let block = Self::override_param(stored, "FILENAME", &filename);
            let block = Self::override_param(&block, "DATE", &date);
            return Self::override_param(&block, "TIME", &time);
        }

        crate::altium::encode_windows1252(&Self::template_library_params(&filename, &date, &time))
    }

    /// Replaces one key's value inside a raw `|KEY=VALUE|` block, leaving every
    /// other byte exactly as it was.
    ///
    /// Works on bytes rather than text because the block is replayed verbatim:
    /// decoding and re-encoding it would round a handful of Windows-1252 byte
    /// values through `?`. A key Altium did not write is appended.
    fn override_param(block: &[u8], key: &str, value: &str) -> Vec<u8> {
        let value = crate::altium::encode_windows1252(value);
        let mut replaced = false;

        let mut segments: Vec<Vec<u8>> = block
            .split(|&b| b == b'|')
            .map(|seg| {
                let matches_key = seg
                    .iter()
                    .position(|&b| b == b'=')
                    .is_some_and(|eq| seg[..eq].eq_ignore_ascii_case(key.as_bytes()));
                if !matches_key {
                    return seg.to_vec();
                }
                replaced = true;
                let mut out = key.as_bytes().to_vec();
                out.push(b'=');
                out.extend_from_slice(&value);
                out
            })
            .collect();

        if !replaced {
            let mut appended = key.as_bytes().to_vec();
            appended.push(b'=');
            appended.extend_from_slice(&value);
            segments.push(appended);
        }

        segments.join(&b'|')
    }

    /// The parameter block a from-scratch library gets.
    ///
    /// Altium Designer requires `VERSION=3.00` plus a V9 layer stack definition
    /// to consider the file valid.
    fn template_library_params(filename: &str, date: &str, time: &str) -> String {
        use std::fmt::Write;

        let mut p = String::with_capacity(4096);

        // Core metadata (must be first, matching AltiumSharp order)
        let _ = write!(p, "|FILENAME={filename}");
        p.push_str("|KIND=Protel_Advanced_PCB_Library");
        p.push_str("|VERSION=3.00");
        let _ = write!(p, "|DATE={date}");
        let _ = write!(p, "|TIME={time}");

        // V9 layer stack + full board configuration. A synthesised stack is
        // rejected by Altium ("Catastrophic failure whilst loading section
        // Library"), so we splice in a complete, known-good stack captured
        // verbatim from a real Altium-authored library.
        p.push('|');
        p.push_str(include_str!("assets/library_data_stack.txt"));

        p
    }

    /// Writes a single footprint to the OLE document.
    ///
    /// # Arguments
    ///
    /// * `cfb` - The OLE compound file
    /// * `footprint` - The footprint to write
    /// * `ole_name` - The OLE-safe storage name (≤31 chars, unique)
    ///
    /// # Streams Created
    ///
    /// - `/{ole_name}/Header` - 4-byte primitive count
    /// - `/{ole_name}/Parameters` - Footprint metadata
    /// - `/{ole_name}/Data` - Binary primitive data
    /// - `/{ole_name}/WideStrings` - Encoded text content
    /// - `/{ole_name}/PrimitiveGuids/Header` - GUID record count
    /// - `/{ole_name}/PrimitiveGuids/Data` - GUIDs for each primitive
    /// - `/{ole_name}/UniqueIDPrimitiveInformation/Header` - UID record count (if applicable)
    /// - `/{ole_name}/UniqueIDPrimitiveInformation/Data` - UID data (if applicable)
    #[allow(clippy::unused_self)] // Method for consistency with other write methods
    fn write_footprint<F: std::io::Read + std::io::Write + std::io::Seek>(
        &self,
        cfb: &mut cfb::CompoundFile<F>,
        footprint: &Footprint,
        ole_name: &str,
    ) -> AltiumResult<()> {
        let storage_path = format!("/{ole_name}");

        // Create storage for the footprint
        crate::altium::create_storage(cfb, &storage_path)?;

        // A footprint unchanged since it was read goes back as Altium wrote
        // it: every stream of its storage, modelled or not, byte for byte.
        if let Some(read) = footprint.unchanged_storage() {
            for (relative, bytes) in &read.streams {
                Self::write_carried_stream(cfb, &storage_path, relative, bytes)?;
            }
            return Ok(());
        }

        // An edited footprint is rebuilt below; the entries of its storage
        // this crate does not model still go back as read rather than being
        // dropped.
        for (relative, bytes) in footprint.unmodelled_streams() {
            tracing::debug!(
                footprint = %footprint.name,
                stream = %relative,
                "Carrying an unmodelled footprint stream as read"
            );
            Self::write_carried_stream(cfb, &storage_path, relative, bytes)?;
        }

        // Write Header stream (4-byte primitive count)
        let header_data = writer::encode_component_header(footprint);
        crate::altium::write_stream(cfb, &format!("{storage_path}/Header"), &header_data)?;

        // Write Parameters stream as a C-string parameter block, in the shape
        // Altium writes (see `build_footprint_params`).
        let params = writer::build_footprint_params(footprint);
        let mut params_data = Vec::new();
        crate::altium::framing::write_cstring_param_block(
            &mut params_data,
            &crate::altium::encode_windows1252(&params),
        );
        crate::altium::write_stream(cfb, &format!("{storage_path}/Parameters"), &params_data)?;

        // Write Data stream with primitives
        let data = Self::encode_primitives(footprint)?;
        crate::altium::write_stream(cfb, &format!("{storage_path}/Data"), &data)?;

        // Write WideStrings stream (per-component)
        let wide_strings_data = writer::encode_component_wide_strings(footprint);
        crate::altium::write_stream(
            cfb,
            &format!("{storage_path}/WideStrings"),
            &wide_strings_data,
        )?;

        // PrimitiveGuids: Altium's stable per-primitive identity. Re-emitted only
        // when the footprint was read from a file that had one — a from-scratch
        // footprint has no identities to preserve, and inventing them would make
        // every save produce different bytes.
        if let Some(guid_data) = writer::encode_primitive_guids(footprint).map(|built| {
            // Altium's own order while the stream holds exactly these records.
            let records = |bytes: &[u8]| {
                let mut chunks: Vec<Vec<u8>> = bytes.chunks(24).map(<[u8]>::to_vec).collect();
                chunks.sort();
                chunks
            };
            match &footprint.primitive_guids_as_read {
                Some(read) if records(read) == records(&built) => read.clone(),
                _ => built,
            }
        }) {
            let guid_storage = format!("{storage_path}/PrimitiveGuids");
            crate::altium::create_storage(cfb, &guid_storage)?;
            // Header = record count; the data is fixed 24-byte records, so the
            // count is exactly what was emitted.
            let count = u32::try_from(guid_data.len() / 24).unwrap_or(u32::MAX);
            crate::altium::write_stream(
                cfb,
                &format!("{guid_storage}/Header"),
                &count.to_le_bytes(),
            )?;
            crate::altium::write_stream(cfb, &format!("{guid_storage}/Data"), &guid_data)?;
        }

        // Write UniqueIDPrimitiveInformation streams if any primitives have unique IDs
        if let Some(uid_data) = writer::encode_unique_id_stream(footprint) {
            // Create UniqueIDPrimitiveInformation storage
            let uid_storage_path = format!("{storage_path}/UniqueIDPrimitiveInformation");
            crate::altium::create_storage(cfb, &uid_storage_path)?;

            // Write Header + Data streams.
            let uid_header_data = writer::encode_unique_id_header(footprint);
            crate::altium::write_stream(
                cfb,
                &format!("{uid_storage_path}/Header"),
                &uid_header_data,
            )?;
            crate::altium::write_stream(cfb, &format!("{uid_storage_path}/Data"), &uid_data)?;

            tracing::trace!(
                footprint = %footprint.name,
                size = uid_data.len(),
                "Wrote UniqueIDPrimitiveInformation streams"
            );
        }

        Ok(())
    }

    /// Writes one stream of a footprint storage as read, creating the
    /// sub-storages its path runs through.
    fn write_carried_stream<F: std::io::Read + std::io::Write + std::io::Seek>(
        cfb: &mut cfb::CompoundFile<F>,
        storage_path: &str,
        relative: &str,
        bytes: &[u8],
    ) -> AltiumResult<()> {
        let mut parent = storage_path.to_string();
        let mut parts: Vec<&str> = relative.split('/').filter(|p| !p.is_empty()).collect();
        let Some(stream) = parts.pop() else {
            return Ok(());
        };
        for part in parts {
            parent = format!("{parent}/{part}");
            if !cfb.is_storage(&parent) {
                crate::altium::create_storage(cfb, &parent)?;
            }
        }
        crate::altium::write_stream(cfb, &format!("{parent}/{stream}"), bytes)
    }

    /// Encodes footprint primitives to binary format.
    ///
    /// See the [`writer`] module for format details.
    ///
    /// # Errors
    ///
    /// Returns an error if any string (footprint name, pad designator, text) exceeds 255 bytes.
    fn encode_primitives(footprint: &Footprint) -> AltiumResult<Vec<u8>> {
        writer::encode_data_stream(footprint)
    }
}

/// A footprint height (mm) as `ComponentParamsTOC` prints it: mils to four
/// decimals with trailing zeros dropped (`1.0` → `39.3701`, `0.6` → `23.622`,
/// `0.0` → `0`).
fn toc_height_mils(height_mm: f64) -> String {
    let text = format!("{:.4}", height_mm / 0.0254);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() || text == "-0" {
        "0".to_string()
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use crate::altium::pcblib::{writer, Footprint, Pad, PcbLib};

    fn temp_dir() -> tempfile::TempDir {
        std::fs::create_dir_all(".tmp").expect("create .tmp");
        let root = std::path::Path::new(".tmp")
            .canonicalize()
            .expect("canonicalise .tmp");
        tempfile::tempdir_in(root).expect("create temp dir")
    }

    /// A footprint keeps the storage it was read from, however its name
    /// would derive today; a carried name that is already taken falls back to
    /// a derived one; a name with `*` derives Altium's `_` form (#507).
    #[test]
    fn a_carried_storage_name_is_kept_and_a_conflicting_one_is_derived() {
        let dir = temp_dir();
        let path = dir.path().join("Carried.PcbLib");
        let mut lib = PcbLib::new();
        for (name, carried) in [
            ("EC6*5.4", Some("LEGACY_STORAGE")),
            ("OTHER", Some("LEGACY_STORAGE")),
            ("EC10*10.5", None),
        ] {
            let mut fp = Footprint::new(name);
            fp.storage_name = carried.map(str::to_string);
            fp.add_pad(Pad::smd("1", 0.0, 0.0, 1.0, 1.0));
            lib.add(fp);
        }
        lib.save(&path).expect("save");

        let cfb = cfb::open(&path).expect("open compound document");
        for storage in ["/LEGACY_STORAGE", "/OTHER", "/EC10_10.5"] {
            assert!(cfb.is_storage(storage), "{storage}");
        }
        assert!(
            !cfb.is_storage("/EC10*10.5"),
            "the star never reaches a storage name"
        );
        drop(cfb);

        let back = PcbLib::open(&path).expect("reopen");
        assert_eq!(back.names(), vec!["EC6*5.4", "OTHER", "EC10*10.5"]);
        assert_eq!(
            back.get("EC6*5.4").unwrap().storage_name.as_deref(),
            Some("LEGACY_STORAGE")
        );
        assert_eq!(
            back.get("EC10*10.5").unwrap().storage_name.as_deref(),
            Some("EC10_10.5")
        );
    }

    /// `SectionKeys` lists a name that reaches the 31-unit cap — as an
    /// identity pair when it merely fills it — and nothing shorter, even a
    /// sanitised name whose storage differs: Altium re-derives that one.
    #[test]
    fn only_names_at_the_storage_cap_are_listed_in_section_keys() {
        let dir = temp_dir();
        let path = dir.path().join("Listing.PcbLib");
        let exactly_31 = "SOP-8_L7.5-W5.9-P1.27-LS11.5-BL";
        assert_eq!(exactly_31.len(), 31);
        let mut lib = PcbLib::new();
        for name in [exactly_31, "EC6*5.4", "SHORT"] {
            let mut fp = Footprint::new(name);
            fp.add_pad(Pad::smd("1", 0.0, 0.0, 1.0, 1.0));
            lib.add(fp);
        }
        lib.save(&path).expect("save");

        let mut cfb = cfb::open(&path).expect("open compound document");
        assert!(cfb.is_storage("/EC6_5.4"));
        let keys = crate::altium::read_stream_opt(&mut cfb, "/SectionKeys").expect("SectionKeys");
        assert_eq!(
            crate::altium::parse_pcblib_section_keys(&keys),
            vec![(exactly_31.to_string(), exactly_31.to_string())]
        );
    }

    /// A footprint past the 31-unit storage cap is stored truncated and
    /// listed in a root `SectionKeys` stream in Altium's binary layout, and
    /// the library reads back in its authored order with the real names
    /// (#507: the text layout of a `SchLib` here left Altium unable to
    /// resolve the mapped footprints).
    #[test]
    fn a_truncated_name_is_mapped_in_a_binary_section_keys_stream() {
        let dir = temp_dir();
        let path = dir.path().join("Truncated.PcbLib");
        let long = "GENERIC_MLCC_CAP_0402_IPC_MEDIUM_DENSITY";
        let mut lib = PcbLib::new();
        for name in [long, "SHORT", "GENERIC_MLCC_CAP_0603_IPC_MEDIUM_DENSITY"] {
            let mut fp = Footprint::new(name);
            fp.add_pad(Pad::smd("1", 0.0, 0.0, 1.0, 1.0));
            lib.add(fp);
        }
        lib.save(&path).expect("save");

        let mut cfb = cfb::open(&path).expect("open compound document");
        assert!(cfb.is_storage("/GENERIC_MLCC_CAP_0402_IPC_MEDIU"));
        let keys = crate::altium::read_stream_opt(&mut cfb, "/SectionKeys").expect("SectionKeys");
        let mut expected = 2u32.to_le_bytes().to_vec();
        for (real, storage) in [
            (long, "GENERIC_MLCC_CAP_0402_IPC_MEDIU"),
            (
                "GENERIC_MLCC_CAP_0603_IPC_MEDIUM_DENSITY",
                "GENERIC_MLCC_CAP_0603_IPC_MEDIU",
            ),
        ] {
            crate::altium::framing::write_string_block(&mut expected, real.as_bytes());
            crate::altium::framing::write_string_block(&mut expected, storage.as_bytes());
        }
        assert_eq!(keys, expected);
        drop(cfb);

        let back = PcbLib::open(&path).expect("reopen");
        assert_eq!(
            back.names(),
            vec![
                long.to_string(),
                "SHORT".to_string(),
                "GENERIC_MLCC_CAP_0603_IPC_MEDIUM_DENSITY".to_string()
            ]
        );
    }

    #[test]
    fn overriding_a_parameter_leaves_every_other_byte_alone() {
        // The block is replayed verbatim, so a rewrite must touch only the one
        // key. Matching is case-insensitive because Altium's own casing varies.
        let block = b"|VERSION=3.00|UNITS=mm|DATE=2020-01-01|";
        let out = PcbLib::override_param(block, "DATE", "2026-08-16");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "|VERSION=3.00|UNITS=mm|DATE=2026-08-16|"
        );

        let out = PcbLib::override_param(block, "version", "4.00");
        assert!(
            String::from_utf8(out).unwrap().contains("version=4.00"),
            "a differently-cased key must still be replaced, not appended"
        );
    }

    #[test]
    fn a_key_altium_did_not_write_is_appended_rather_than_dropped() {
        // Silently dropping it would write a library missing the parameter the
        // caller asked for, with no error to show for it.
        let out = PcbLib::override_param(b"|VERSION=3.00|", "UNITS", "mm");
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("|VERSION=3.00|"), "{text}");
        assert!(text.ends_with("UNITS=mm"), "{text}");
    }

    /// A footprint named outside Windows-1252 from scratch — #507's canary,
    /// `CANARY*X/（0402）×` — is stored the way Altium stores it: under the real
    /// name, sanitised, with the name's UTF-16 code units in a `UNICODE__PATTERN`
    /// twin Altium reads the name from and derives that storage from; and it
    /// reads back under its real name.
    #[test]
    fn a_new_non_ascii_name_gets_altiums_storage_and_a_unicode_twin() {
        let dir = temp_dir();
        let path = dir.path().join("Canary.PcbLib");
        let name = "CANARY*X/\u{FF08}0402\u{FF09}\u{D7}";
        let mut lib = PcbLib::new();
        let mut fp = Footprint::new(name);
        fp.add_pad(Pad::smd("1", 0.0, 0.0, 1.0, 1.0));
        lib.add(fp);
        lib.save(&path).expect("save");

        let storage = "/CANARY_X_\u{FF08}0402\u{FF09}\u{D7}";
        let mut cfb = cfb::open(&path).expect("open compound document");
        assert!(
            cfb.is_storage(storage),
            "stored under the real name, sanitised"
        );
        let params = crate::altium::read_stream_opt(&mut cfb, format!("{storage}/Parameters"))
            .expect("Parameters");
        let text = crate::altium::decode_windows1252(&params[4..]);
        assert_eq!(
            text.trim_end_matches('\0'),
            "|UNICODE=EXISTS|PATTERN=CANARY*X/?0402?\u{D7}|HEIGHT=0mil|DESCRIPTION=|ITEMGUID=\
             |REVISIONGUID=|UNICODE__PATTERN=67,65,78,65,82,89,42,88,47,65288,48,52,48,50,65289,215\
             |UNICODE=EXISTS"
        );
        assert!(
            crate::altium::read_stream_opt(&mut cfb, "/SectionKeys").is_none(),
            "a name within the cap is not listed"
        );

        let reread = PcbLib::open(&path).expect("read back");
        let fp = reread.get(name).expect("found by its real name");
        assert_eq!(fp.storage_name.as_deref(), Some(&storage[1..]));
        assert!(
            fp.additional_parameters.is_empty(),
            "a block the writer regenerates is not carried"
        );
    }

    /// The footprint height is written as Altium's mil string and read back.
    #[test]
    fn a_footprint_height_survives_a_write_read_cycle() {
        let dir = temp_dir();
        let path = dir.path().join("Height.PcbLib");
        let mut lib = PcbLib::new();
        let mut fp = Footprint::new("TALL");
        fp.height = 2.5;
        fp.add_pad(Pad::smd("1", 0.0, 0.0, 1.0, 1.0));
        lib.add(fp);
        lib.save(&path).expect("save");

        let mut cfb = cfb::open(&path).expect("open compound document");
        let params =
            crate::altium::read_stream_opt(&mut cfb, "/TALL/Parameters").expect("Parameters");
        assert!(
            crate::altium::decode_windows1252(&params).contains("|HEIGHT=98.425197mil|"),
            "2.5 mm in mils"
        );
        let reread = PcbLib::open(&path).expect("read back");
        let tall = reread.get("TALL").expect("TALL");
        assert!((tall.height - 2.5).abs() < 1e-6, "{}", tall.height);
        assert!(tall.additional_parameters.is_empty());
    }

    /// A library in a GBK code page reads its ANSI-only text — here a pad
    /// designator — as itself, and a rewrite keeps every byte.
    #[test]
    fn a_gbk_library_reads_its_ansi_text_and_rewrites_it_byte_identically() {
        let dir = temp_dir();
        let path = dir.path().join("Gbk.PcbLib");
        let name = "DFN\u{FF08}0402\u{FF09}";
        let mut lib = PcbLib::new();
        lib.metadata.ansi_code_page = Some(936);
        let mut fp = Footprint::new(name);
        fp.add_pad(Pad::smd("\u{FF08}1\u{FF09}", 0.0, 0.0, 1.0, 1.0));
        lib.add(fp);
        lib.save(&path).expect("save");

        let data_of = |p: &std::path::Path| {
            let mut cfb = cfb::open(p).expect("open compound document");
            crate::altium::read_stream_opt(&mut cfb, format!("/{name}/Data")).expect("Data")
        };
        assert!(
            data_of(&path).windows(5).any(|w| w == b"\xA3\xA81\xA3\xA9"),
            "the designator is written in GBK"
        );

        let mut back = PcbLib::open(&path).expect("read back");
        assert_eq!(back.metadata().ansi_code_page, Some(936), "detected");
        assert_eq!(
            back.get(name).expect("footprint").pads[0].designator,
            "\u{FF08}1\u{FF09}"
        );

        let again = dir.path().join("Again.PcbLib");
        back.save(&again).expect("rewrite");
        assert_eq!(data_of(&again), data_of(&path), "byte-identical rewrite");
    }

    /// A footprint carried from a GBK library keeps its GBK bytes in a library
    /// of that page, but is rebuilt in a Windows-1250 one rather than carrying
    /// bytes Altium would read as something else there.
    #[test]
    fn a_footprint_carried_across_code_pages_is_rewritten_in_the_target_page() {
        let mut fp = Footprint::new("DFN\u{FF08}0402\u{FF09}");
        let gbk = crate::altium::decode_windows1252(b"DFN\xA3\xA80402\xA3\xA9");
        fp.additional_parameters = vec![
            ("PATTERN".to_string(), gbk.clone()),
            ("DESCRIPTION".to_string(), String::new()),
            (
                "UNICODE__PATTERN".to_string(),
                crate::altium::utf16_units_decimal(&fp.name),
            ),
        ];
        fp.param_key_order = ["PATTERN", "HEIGHT", "DESCRIPTION", "UNICODE__PATTERN"]
            .iter()
            .map(|k| (*k).to_string())
            .collect();
        let in_gbk = crate::altium::with_ansi_encoding(encoding_rs::GBK, || {
            writer::footprint_pattern_text(&fp)
        });
        assert_eq!(in_gbk, gbk);
        let in_1250 = crate::altium::with_ansi_encoding(encoding_rs::WINDOWS_1250, || {
            writer::footprint_pattern_text(&fp)
        });
        assert_eq!(in_1250, "DFN?0402?", "rebuilt as Windows-1250 husks");
    }

    /// A library built in memory from footprints read elsewhere takes the code
    /// page their carried bytes were written in, so replaying them through a
    /// new library stays byte-identical.
    #[test]
    fn a_new_library_takes_the_code_page_of_its_carried_footprints() {
        let mut fp = Footprint::new("DFN\u{FF08}0402\u{FF09}");
        fp.additional_parameters = vec![
            (
                "PATTERN".to_string(),
                crate::altium::decode_windows1252(b"DFN\xA3\xA80402\xA3\xA9"),
            ),
            (
                "UNICODE__PATTERN".to_string(),
                crate::altium::utf16_units_decimal(&fp.name),
            ),
        ];
        let mut lib = PcbLib::new();
        lib.add(fp);
        assert_eq!(lib.ansi_encoding(), encoding_rs::GBK);
        assert_eq!(PcbLib::new().ansi_encoding(), encoding_rs::WINDOWS_1252);
    }

    /// Every stream under `/{storage}`, by its path inside the storage.
    fn storage_streams(path: &std::path::Path, storage: &str) -> Vec<(String, Vec<u8>)> {
        use std::io::Read as _;
        let mut cfb = cfb::open(path).expect("open compound document");
        let root = format!("/{storage}");
        // Collected first: the walk borrows the file that each stream is
        // then opened from.
        #[allow(clippy::needless_collect)]
        let paths: Vec<String> = cfb
            .walk_storage(&root)
            .expect("walk storage")
            .filter(cfb::Entry::is_stream)
            .map(|e| e.path().to_string_lossy().into_owned())
            .collect();
        let mut streams: Vec<(String, Vec<u8>)> = paths
            .into_iter()
            .map(|p| {
                let mut bytes = Vec::new();
                cfb.open_stream(&p)
                    .expect("open stream")
                    .read_to_end(&mut bytes)
                    .expect("read stream");
                (p[root.len()..].to_string(), bytes)
            })
            .collect();
        streams.sort();
        streams
    }

    /// Two footprints saved by this crate, the first given a sub-storage it
    /// does not model — as Altium gives some footprints an
    /// `ExtendedPrimitiveInformation` storage with its own `Data` stream.
    fn library_with_an_unmodelled_sub_storage(path: &std::path::Path) {
        use std::io::Write as _;
        let mut lib = PcbLib::new();
        for name in ["KEEP", "EDIT"] {
            let mut fp = Footprint::new(name);
            fp.add_pad(Pad::smd("1", 0.0, 0.0, 1.0, 1.0));
            lib.add(fp);
        }
        lib.save(path).expect("save");
        let mut cfb = cfb::open_rw(path).expect("open for writing");
        cfb.create_storage("/KEEP/ExtendedPrimitiveInformation")
            .expect("create sub-storage");
        for (stream, bytes) in [("Header", &b"\x01\x00\x00\x00"[..]), ("Data", b"EXTRA")] {
            cfb.create_stream(format!("/KEEP/ExtendedPrimitiveInformation/{stream}"))
                .expect("create stream")
                .write_all(bytes)
                .expect("write stream");
        }
        cfb.flush().expect("flush");
    }

    /// A footprint's own sub-storage is never read as a footprint, and an
    /// untouched footprint goes back byte for byte — the sub-storage this
    /// crate does not model included — when another one is edited.
    #[test]
    fn an_untouched_footprint_and_its_unmodelled_streams_survive_a_rewrite() {
        let dir = temp_dir();
        let path = dir.path().join("Carry.PcbLib");
        library_with_an_unmodelled_sub_storage(&path);
        let before = storage_streams(&path, "KEEP");
        assert!(before
            .iter()
            .any(|(p, _)| p == "/ExtendedPrimitiveInformation/Data"));

        let mut lib = PcbLib::open(&path).expect("open");
        let names: Vec<&str> = lib.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["KEEP", "EDIT"], "a sub-storage is not a footprint");

        let edit = lib.get_mut("EDIT").expect("EDIT");
        edit.pads[0].width = 2.5;
        lib.save(&path).expect("save");

        assert_eq!(
            storage_streams(&path, "KEEP"),
            before,
            "KEEP comes back verbatim"
        );
        let read = PcbLib::open(&path).expect("reopen");
        let width = read.get("EDIT").expect("EDIT").pads[0].width;
        assert!(
            (width - 2.5).abs() < 1e-6,
            "the edit is written, not the old storage"
        );
        assert_eq!(read.iter().count(), 2);
    }

    /// An edited footprint is rebuilt, and the sub-storage this crate does
    /// not model still goes back as read rather than being dropped.
    #[test]
    fn an_edited_footprint_keeps_its_unmodelled_streams() {
        let dir = temp_dir();
        let path = dir.path().join("CarryEdited.PcbLib");
        library_with_an_unmodelled_sub_storage(&path);

        let mut lib = PcbLib::open(&path).expect("open");
        lib.get_mut("KEEP").expect("KEEP").pads[0].height = 3.0;
        lib.save(&path).expect("save");

        let after = storage_streams(&path, "KEEP");
        let extra = after
            .iter()
            .find(|(p, _)| p == "/ExtendedPrimitiveInformation/Data")
            .map(|(_, b)| b.as_slice());
        assert_eq!(extra, Some(&b"EXTRA"[..]));
        let read = PcbLib::open(&path).expect("reopen");
        assert!((read.get("KEEP").expect("KEEP").pads[0].height - 3.0).abs() < 1e-4);
    }

    /// `ComponentParamsTOC` gives a footprint its height in mils as Altium
    /// prints it, not 0.
    #[test]
    fn the_component_params_toc_carries_the_footprint_height() {
        let mut lib = PcbLib::new();
        for (name, height) in [("ONE", 1.0), ("POINT6", 0.6), ("FLAT", 0.0)] {
            let mut fp = Footprint::new(name);
            fp.height = height;
            fp.add_pad(Pad::smd("1", 0.0, 0.0, 1.0, 1.0));
            lib.add(fp);
        }
        let toc = String::from_utf8_lossy(&lib.component_params_toc_stream()).into_owned();
        assert!(
            toc.contains("Name=ONE|Pad Count=1|Height=39.3701|"),
            "{toc}"
        );
        assert!(
            toc.contains("Name=POINT6|Pad Count=1|Height=23.622|"),
            "{toc}"
        );
        assert!(toc.contains("Name=FLAT|Pad Count=1|Height=0|"), "{toc}");
    }

    /// A body supplied beside a STEP model to embed is the body the model is
    /// embedded into: its layer, height and outline are kept, not replaced by
    /// a default body.
    #[test]
    fn a_supplied_body_receives_the_embedded_step_model() {
        use crate::altium::pcblib::{ComponentBody, Layer, Model3D};
        let dir = temp_dir();
        let step = dir.path().join("BODY.step");
        std::fs::write(&step, b"ISO-10303-21;\nEND-ISO-10303-21;\n").expect("write step");
        let path = dir.path().join("Body.PcbLib");

        let mut fp = Footprint::new("WITH_BODY");
        fp.add_pad(Pad::smd("1", 0.0, 0.0, 1.0, 1.0));
        let mut body = ComponentBody::new("", "BODY.step");
        body.layer = Layer::Mechanical13;
        body.overall_height = 1.0;
        body.outline = vec![(-1.75, -1.75), (1.75, -1.75), (1.75, 1.75), (-1.75, 1.75)];
        fp.component_bodies.push(body);
        fp.model_3d = Some(Model3D {
            filepath: step.to_string_lossy().into_owned(),
            x_offset: 0.0,
            y_offset: 0.0,
            z_offset: 0.0,
            rotation: 0.0,
        });
        let mut lib = PcbLib::new();
        lib.add(fp);
        lib.save(&path).expect("save");

        let read = PcbLib::open(&path).expect("reopen");
        let bodies = &read.get("WITH_BODY").expect("footprint").component_bodies;
        assert_eq!(bodies.len(), 1, "no second, default body");
        let body = &bodies[0];
        assert_eq!(body.layer, Layer::Mechanical13);
        assert!((body.overall_height - 1.0).abs() < 1e-4);
        assert_eq!(body.outline.len(), 4);
        assert!(body.embedded);
        assert!(read
            .models()
            .any(|m| m.id.eq_ignore_ascii_case(&body.model_id)));
    }
}
