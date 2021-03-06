use gl;
use GlObject;

use backend::Facade;
use version::Version;
use context::Context;
use context::CommandContext;
use ContextExt;
use TextureExt;
use TextureMipmapExt;
use version::Api;
use Rect;

use pixel_buffer::PixelBuffer;
use image_format::{self, TextureFormatRequest, ClientFormatAny};
use texture::Texture2dDataSink;
use texture::{MipmapsOption, TextureFormat, TextureCreationError};
use texture::{get_format, InternalFormat, GetFormatError};

use buffer::BufferViewAny;
use BufferViewExt;

use libc;
use std::cmp;
use std::fmt;
use std::mem;
use std::ptr;
use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;

use ops;
use fbo;

/// A texture whose type isn't fixed at compile-time.
pub struct TextureAny {
    context: Rc<Context>,
    id: gl::types::GLuint,
    requested_format: TextureFormatRequest,

    /// Cache for the actual format of the texture. The outer Option is None if the format hasn't
    /// been checked yet. The inner Result is Err if the format has been checkek but is unknown.
    actual_format: Cell<Option<Result<InternalFormat, GetFormatError>>>,

    /// Type and dimensions of the texture.
    ty: Dimensions,

    /// Number of mipmap levels (`1` means just the main texture, `0` is not valid)
    levels: u32,
    /// Is automatic mipmap generation allowed for this texture?
    generate_mipmaps: bool,
}

/// Represents a specific mipmap of a texture.
#[derive(Copy, Clone)]
pub struct TextureAnyMipmap<'a> {
    /// The texture.
    texture: &'a TextureAny,

    /// Layer for array textures, or 0 for other textures.
    layer: u32,

    /// Mipmap level.
    level: u32,

    /// Width of this mipmap level.
    width: u32,
    /// Height of this mipmap level.
    height: Option<u32>,
    /// Depth of this mipmap level.
    depth: Option<u32>,
}

/// Type of a texture.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[allow(missing_docs)]      // TODO: document and remove
pub enum Dimensions {
    Texture1d { width: u32 },
    Texture1dArray { width: u32, array_size: u32 },
    Texture2d { width: u32, height: u32 },
    Texture2dArray { width: u32, height: u32, array_size: u32 },
    Texture2dMultisample { width: u32, height: u32, samples: u32 },
    Texture2dMultisampleArray { width: u32, height: u32, array_size: u32, samples: u32 },
    Texture3d { width: u32, height: u32, depth: u32 },
}

/// Builds a new texture.
///
/// # Panic
///
/// Panicks if the size of the data doesn't match the texture dimensions.
pub fn new_texture<'a, F, P>(facade: &F, format: TextureFormatRequest,
                             data: Option<(ClientFormatAny, Cow<'a, [P]>)>,
                             mipmaps: MipmapsOption, ty: Dimensions)
                             -> Result<TextureAny, TextureCreationError>
                             where P: Send + Clone + 'a, F: Facade
{
    // getting the width, height, depth, array_size, samples from the type
    let (width, height, depth, array_size, samples) = match ty {
        Dimensions::Texture1d { width } => (width, None, None, None, None),
        Dimensions::Texture1dArray { width, array_size } => (width, None, None, Some(array_size), None),
        Dimensions::Texture2d { width, height } => (width, Some(height), None, None, None),
        Dimensions::Texture2dArray { width, height, array_size } => (width, Some(height), None, Some(array_size), None),
        Dimensions::Texture2dMultisample { width, height, samples } => (width, Some(height), None, None, Some(samples)),
        Dimensions::Texture2dMultisampleArray { width, height, array_size, samples } => (width, Some(height), None, Some(array_size), Some(samples)),
        Dimensions::Texture3d { width, height, depth } => (width, Some(height), Some(depth), None, None),
    };

    let (is_client_compressed, data_bufsize) = match data {
        Some((client_format, _)) => {
            (client_format.is_compressed(),
             client_format.get_buffer_size(width, height, depth, array_size))
        },
        None => (false, 0),
    };

    if let Some((_, ref data)) = data {
        if data.len() * mem::size_of::<P>() != data_bufsize
        {
            panic!("Texture data size mismatch");
        }
    }

    // getting the `GLenum` corresponding to this texture type
    let bind_point = match ty {
        Dimensions::Texture1d { .. } => gl::TEXTURE_1D,
        Dimensions::Texture1dArray { .. } => gl::TEXTURE_1D_ARRAY,
        Dimensions::Texture2d { .. } => gl::TEXTURE_2D,
        Dimensions::Texture2dArray { .. } => gl::TEXTURE_2D_ARRAY,
        Dimensions::Texture2dMultisample { .. } => gl::TEXTURE_2D_MULTISAMPLE,
        Dimensions::Texture2dMultisampleArray { .. } => gl::TEXTURE_2D_MULTISAMPLE_ARRAY,
        Dimensions::Texture3d { .. } => gl::TEXTURE_3D,
    };

    // checking non-power-of-two
    if facade.get_context().get_version() < &Version(Api::Gl, 2, 0) &&
        !facade.get_context().get_extensions().gl_arb_texture_non_power_of_two
    {
        if !width.is_power_of_two() || !height.unwrap_or(2).is_power_of_two() ||
            !depth.unwrap_or(2).is_power_of_two() || !array_size.unwrap_or(2).is_power_of_two()
        {
            return Err(TextureCreationError::DimensionsNotSupported);
        }
    }

    let generate_mipmaps = mipmaps.should_generate();
    let texture_levels = mipmaps.num_levels(width, height, depth) as gl::types::GLsizei;

    let (teximg_internal_format, storage_internal_format) =
        try!(image_format::format_request_to_glenum(facade.get_context(), data.as_ref().map(|&(c, _)| c), format));

    let (client_format, client_type) = match (&data, format) {
        (&Some((client_format, _)), f) => try!(image_format::client_format_to_glenum(facade.get_context(), client_format, f)),
        (&None, TextureFormatRequest::AnyDepth) => (gl::DEPTH_COMPONENT, gl::FLOAT),
        (&None, TextureFormatRequest::Specific(TextureFormat::DepthFormat(_))) => (gl::DEPTH_COMPONENT, gl::FLOAT),
        (&None, TextureFormatRequest::AnyDepthStencil) => (gl::DEPTH_STENCIL, gl::UNSIGNED_INT_24_8),
        (&None, TextureFormatRequest::Specific(TextureFormat::DepthStencilFormat(_))) => (gl::DEPTH_STENCIL, gl::UNSIGNED_INT_24_8),
        (&None, _) => (gl::RGBA, gl::UNSIGNED_BYTE),
    };

    let mut ctxt = facade.get_context().make_current();

    let id = unsafe {
        let has_mipmaps = texture_levels > 1;
        let data = data;
        let data_raw = if let Some((_, ref data)) = data {
            data.as_ptr() as *const libc::c_void
        } else {
            ptr::null()
        };

        if ctxt.state.pixel_store_unpack_alignment != 1 {
            ctxt.state.pixel_store_unpack_alignment = 1;
            ctxt.gl.PixelStorei(gl::UNPACK_ALIGNMENT, 1);
        }

        BufferViewAny::unbind_pixel_unpack(&mut ctxt);

        let id: gl::types::GLuint = mem::uninitialized();
        ctxt.gl.GenTextures(1, mem::transmute(&id));

        {
            ctxt.gl.BindTexture(bind_point, id);
            let act = ctxt.state.active_texture as usize;
            ctxt.state.texture_units[act].texture = id;
        }

        ctxt.gl.TexParameteri(bind_point, gl::TEXTURE_WRAP_S, gl::REPEAT as i32);
        ctxt.gl.TexParameteri(bind_point, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);

        match ty {
            Dimensions::Texture1d { .. } => (),
            _ => {
                ctxt.gl.TexParameteri(bind_point, gl::TEXTURE_WRAP_T, gl::REPEAT as i32);
            },
        };

        match ty {
            Dimensions::Texture1d { .. } => (),
            Dimensions::Texture2d { .. } => (),
            Dimensions::Texture2dMultisample { .. } => (),
            _ => {
                ctxt.gl.TexParameteri(bind_point, gl::TEXTURE_WRAP_R, gl::REPEAT as i32);
            },
        };

        if has_mipmaps {
            ctxt.gl.TexParameteri(bind_point, gl::TEXTURE_MIN_FILTER,
                                  gl::LINEAR_MIPMAP_LINEAR as i32);
        } else {
            ctxt.gl.TexParameteri(bind_point, gl::TEXTURE_MIN_FILTER,
                                  gl::LINEAR as i32);
        }

        if !has_mipmaps && (ctxt.version >= &Version(Api::Gl, 1, 2) ||
                            ctxt.version >= &Version(Api::GlEs, 3, 0))
        {
            ctxt.gl.TexParameteri(bind_point, gl::TEXTURE_BASE_LEVEL, 0);
            ctxt.gl.TexParameteri(bind_point, gl::TEXTURE_MAX_LEVEL, 0);
        }

        if bind_point == gl::TEXTURE_3D || bind_point == gl::TEXTURE_2D_ARRAY {
            let mut data_raw = data_raw;

            let width = match width as gl::types::GLsizei {
                0 => { data_raw = ptr::null(); 1 },
                a => a
            };

            let height = match height.unwrap() as gl::types::GLsizei {
                0 => { data_raw = ptr::null(); 1 },
                a => a
            };

            let depth = match depth.or(array_size).unwrap() as gl::types::GLsizei {
                0 => { data_raw = ptr::null(); 1 },
                a => a
            };

            if storage_internal_format.is_some() && (ctxt.version >= &Version(Api::Gl, 4, 2) || ctxt.extensions.gl_arb_texture_storage) {
                ctxt.gl.TexStorage3D(bind_point, texture_levels,
                                     storage_internal_format.unwrap() as gl::types::GLenum,
                                     width, height, depth);

                if !data_raw.is_null() {
                    if is_client_compressed {
                        ctxt.gl.CompressedTexSubImage3D(bind_point, 0, 0, 0, 0, width, height, depth,
                                                         teximg_internal_format as u32,
                                                         data_bufsize as i32, data_raw);
                    } else {
                        ctxt.gl.TexSubImage3D(bind_point, 0, 0, 0, 0, width, height, depth,
                                              client_format, client_type, data_raw);
                    }
                }

            } else {
                if is_client_compressed && !data_raw.is_null() {
                    ctxt.gl.CompressedTexImage3D(bind_point, 0, teximg_internal_format as u32, 
                                       width, height, depth, 0, data_bufsize as i32, data_raw);
                } else {
                    ctxt.gl.TexImage3D(bind_point, 0, teximg_internal_format as i32, width,
                                       height, depth, 0, client_format as u32, client_type,
                                       data_raw);
                }
            }

        } else if bind_point == gl::TEXTURE_2D || bind_point == gl::TEXTURE_1D_ARRAY {
            let mut data_raw = data_raw;

            let width = match width as gl::types::GLsizei {
                0 => { data_raw = ptr::null(); 1 },
                a => a
            };

            let height = match height.or(array_size).unwrap() as gl::types::GLsizei {
                0 => { data_raw = ptr::null(); 1 },
                a => a
            };

            if storage_internal_format.is_some() && (ctxt.version >= &Version(Api::Gl, 4, 2) || ctxt.extensions.gl_arb_texture_storage) {
                ctxt.gl.TexStorage2D(bind_point, texture_levels,
                                     storage_internal_format.unwrap() as gl::types::GLenum,
                                     width, height);

                if !data_raw.is_null() {
                    if is_client_compressed {
                        ctxt.gl.CompressedTexSubImage2D(bind_point, 0, 0, 0, width, height,
                                                         teximg_internal_format as u32,
                                                         data_bufsize as i32, data_raw);
                    } else {
                        ctxt.gl.TexSubImage2D(bind_point, 0, 0, 0, width, height, client_format,
                                              client_type, data_raw);
                    }
                }

            } else {
                if is_client_compressed && !data_raw.is_null() {
                    ctxt.gl.CompressedTexImage2D(bind_point, 0, teximg_internal_format as u32, 
                                       width, height, 0, data_bufsize as i32, data_raw);
                } else {
                    ctxt.gl.TexImage2D(bind_point, 0, teximg_internal_format as i32, width,
                                       height, 0, client_format as u32, client_type, data_raw);
                }
            }

        } else if bind_point == gl::TEXTURE_2D_MULTISAMPLE {
            assert!(data_raw.is_null());

            let width = match width as gl::types::GLsizei {
                0 => 1,
                a => a
            };

            let height = match height.unwrap() as gl::types::GLsizei {
                0 => 1,
                a => a
            };

            if storage_internal_format.is_some() && (ctxt.version >= &Version(Api::Gl, 4, 2) || ctxt.extensions.gl_arb_texture_storage) {
                ctxt.gl.TexStorage2DMultisample(gl::TEXTURE_2D_MULTISAMPLE,
                                                samples.unwrap() as gl::types::GLsizei,
                                                storage_internal_format.unwrap() as gl::types::GLenum,
                                                width, height, gl::TRUE);

            } else if ctxt.version >= &Version(Api::Gl, 3, 2) || ctxt.extensions.gl_arb_texture_multisample {
                ctxt.gl.TexImage2DMultisample(gl::TEXTURE_2D_MULTISAMPLE,
                                              samples.unwrap() as gl::types::GLsizei,
                                              teximg_internal_format as gl::types::GLenum,
                                              width, height, gl::TRUE);

            } else {
                unreachable!();
            }

        } else if bind_point == gl::TEXTURE_2D_MULTISAMPLE_ARRAY {
            assert!(data_raw.is_null());

            let width = match width as gl::types::GLsizei {
                0 => 1,
                a => a
            };

            let height = match height.unwrap() as gl::types::GLsizei {
                0 => 1,
                a => a
            };

            if storage_internal_format.is_some() && (ctxt.version >= &Version(Api::Gl, 4, 2) || ctxt.extensions.gl_arb_texture_storage) {
                ctxt.gl.TexStorage3DMultisample(gl::TEXTURE_2D_MULTISAMPLE_ARRAY,
                                                samples.unwrap() as gl::types::GLsizei,
                                                storage_internal_format.unwrap() as gl::types::GLenum,
                                                width, height, array_size.unwrap() as gl::types::GLsizei,
                                                gl::TRUE);

            } else if ctxt.version >= &Version(Api::Gl, 3, 2) || ctxt.extensions.gl_arb_texture_multisample {
                ctxt.gl.TexImage3DMultisample(gl::TEXTURE_2D_MULTISAMPLE_ARRAY,
                                              samples.unwrap() as gl::types::GLsizei,
                                              teximg_internal_format as gl::types::GLenum,
                                              width, height, array_size.unwrap() as gl::types::GLsizei,
                                              gl::TRUE);

            } else {
                unreachable!();
            }

        } else if bind_point == gl::TEXTURE_1D {
            let mut data_raw = data_raw;

            let width = match width as gl::types::GLsizei {
                0 => { data_raw = ptr::null(); 1 },
                a => a
            };

            if storage_internal_format.is_some() && (ctxt.version >= &Version(Api::Gl, 4, 2) || ctxt.extensions.gl_arb_texture_storage) {
                ctxt.gl.TexStorage1D(bind_point, texture_levels,
                                     storage_internal_format.unwrap() as gl::types::GLenum,
                                     width);

                if !data_raw.is_null() {
                    if is_client_compressed {
                        ctxt.gl.CompressedTexSubImage1D(bind_point, 0, 0, width,
                                                         teximg_internal_format as u32,
                                                         data_bufsize as i32, data_raw);
                    } else {
                        ctxt.gl.TexSubImage1D(bind_point, 0, 0, width, client_format,
                                              client_type, data_raw);
                    }
                }

            } else {
                if is_client_compressed && !data_raw.is_null() {
                    ctxt.gl.CompressedTexImage1D(bind_point, 0, teximg_internal_format as u32, 
                                       width, 0, data_bufsize as i32, data_raw);
                } else {
                    ctxt.gl.TexImage1D(bind_point, 0, teximg_internal_format as i32, width,
                                       0, client_format as u32, client_type, data_raw);
                }
            }

        } else {
            unreachable!();
        }

        // only generate mipmaps for color textures
        if generate_mipmaps {
            if ctxt.version >= &Version(Api::Gl, 3, 0) ||
               ctxt.version >= &Version(Api::GlEs, 2, 0)
            {
                ctxt.gl.GenerateMipmap(bind_point);
            } else if ctxt.extensions.gl_ext_framebuffer_object {
                ctxt.gl.GenerateMipmapEXT(bind_point);
            } else {
                unreachable!();
            }
        }

        id
    };

    Ok(TextureAny {
        context: facade.get_context().clone(),
        id: id,
        requested_format: format,
        actual_format: Cell::new(None),
        ty: ty,
        levels: texture_levels as u32,
        generate_mipmaps: generate_mipmaps,
    })
}

impl<'a> TextureAnyMipmap<'a> {
    /// Returns the texture.
    pub fn get_texture(&self) -> &'a TextureAny {
        self.texture
    }

    /// Returns the level of the texture.
    pub fn get_level(&self) -> u32 {
        self.level
    }

    /// Returns the layer of the texture.
    pub fn get_layer(&self) -> u32 {
        self.layer
    }
}

impl<'t> TextureMipmapExt for TextureAnyMipmap<'t> {
    fn read<T>(&self) -> T where T: Texture2dDataSink<(u8, u8, u8, u8)> {
        let attachment = fbo::Attachment::Texture {
            texture: &self.texture,
            layer: Some(self.layer),
            level: self.level,
        };

        let rect = Rect {
            bottom: 0,
            left: 0,
            width: self.width,
            height: self.height.unwrap_or(1),
        };

        let mut ctxt = self.texture.context.make_current();

        let mut data = Vec::with_capacity(0);
        ops::read(&mut ctxt, &attachment, &rect, &mut data);
        T::from_raw(Cow::Owned(data), self.width, self.height.unwrap_or(1))
    }

    fn read_to_pixel_buffer(&self) -> PixelBuffer<(u8, u8, u8, u8)> {
        let size = self.width as usize * self.height.unwrap_or(1) as usize * 4;

        let attachment = fbo::Attachment::Texture {
            texture: &self.texture,
            layer: Some(self.layer),
            level: self.level,
        };

        let rect = Rect {
            bottom: 0,
            left: 0,
            width: self.width,
            height: self.height.unwrap_or(1),
        };

        let pb = PixelBuffer::new_empty(&self.texture.context, size);

        let mut ctxt = self.texture.context.make_current();
        ops::read(&mut ctxt, &attachment, &rect, &pb);
        pb
    }

    fn upload_texture<'d, P>(&self, x_offset: u32, y_offset: u32, z_offset: u32,
                             (format, data): (ClientFormatAny, Cow<'d, [P]>), width: u32,
                             height: Option<u32>, depth: Option<u32>,
                             regen_mipmaps: bool)
                             -> Result<(), ()>   // TODO return a better Result!?
                             where P: Send + Copy + Clone + 'd
    {
        let id = self.texture.id;
        let level = self.level;

        let (is_client_compressed, data_bufsize) = (format.is_compressed(),
                                                    format.get_buffer_size(width, height, depth, None));
        let regen_mipmaps = regen_mipmaps && self.texture.levels >= 2 &&
                            self.texture.generate_mipmaps && !is_client_compressed;

        assert!(!regen_mipmaps || level == 0);  // when regen_mipmaps is true, level must be 0!
        assert!(x_offset <= self.width);
        assert!(y_offset <= self.height.unwrap_or(1));
        assert!(z_offset <= self.depth.unwrap_or(1));
        assert!(x_offset + width <= self.width);
        assert!(y_offset + height.unwrap_or(1) <= self.height.unwrap_or(1));
        assert!(z_offset + depth.unwrap_or(1) <= self.depth.unwrap_or(1));

        if data.len() * mem::size_of::<P>() != data_bufsize
        {
            panic!("Texture data size mismatch");
        }

        let (client_format, client_type) = try!(image_format::client_format_to_glenum(&self.texture.context,
                                                                                      format,
                                                                                      self.texture.requested_format)
                                                                                      .map_err(|_| ()));

        let mut ctxt = self.texture.context.make_current();

        unsafe {
            if ctxt.state.pixel_store_unpack_alignment != 1 {
                ctxt.state.pixel_store_unpack_alignment = 1;
                ctxt.gl.PixelStorei(gl::UNPACK_ALIGNMENT, 1);
            }

            BufferViewAny::unbind_pixel_unpack(&mut ctxt);
            let bind_point = self.texture.bind_to_current(&mut ctxt);

            if bind_point == gl::TEXTURE_3D || bind_point == gl::TEXTURE_2D_ARRAY {
                unimplemented!();

            } else if bind_point == gl::TEXTURE_2D || bind_point == gl::TEXTURE_1D_ARRAY {
                assert!(z_offset == 0);
                // FIXME should glTexImage be used here somewhere or glTexSubImage does it just fine?
                if is_client_compressed {
                    ctxt.gl.CompressedTexSubImage2D(bind_point, level as gl::types::GLint,
                                                    x_offset as gl::types::GLint,
                                                    y_offset as gl::types::GLint,
                                                    width as gl::types::GLsizei,
                                                    height.unwrap_or(1) as gl::types::GLsizei,
                                                    client_format,
                                                    data_bufsize  as gl::types::GLsizei,
                                                    data.as_ptr() as *const libc::c_void);
                } else {
                    ctxt.gl.TexSubImage2D(bind_point, level as gl::types::GLint,
                                          x_offset as gl::types::GLint,
                                          y_offset as gl::types::GLint,
                                          width as gl::types::GLsizei,
                                          height.unwrap_or(1) as gl::types::GLsizei,
                                          client_format, client_type,
                                          data.as_ptr() as *const libc::c_void);
                }

            } else {
                assert!(z_offset == 0);
                assert!(y_offset == 0);

                unimplemented!();
            }

            // regenerate mipmaps if there are some
            if regen_mipmaps {
                if ctxt.version >= &Version(Api::Gl, 3, 0) {
                    ctxt.gl.GenerateMipmap(bind_point);
                } else {
                    ctxt.gl.GenerateMipmapEXT(bind_point);
                }
            }

            Ok(())
        }
    }

    fn download_compressed_data(&self) -> Option<(ClientFormatAny, Vec<u8>)> {
        let texture = self.texture;
        let level = self.level as i32;

        let mut ctxt = texture.context.make_current();

        unsafe {
            let bind_point = texture.bind_to_current(&mut ctxt);

            let mut is_compressed = mem::uninitialized();
            ctxt.gl.GetTexLevelParameteriv(bind_point, level, gl::TEXTURE_COMPRESSED, &mut is_compressed);
            if is_compressed != 0 {

                let mut buffer_size = mem::uninitialized();
                ctxt.gl.GetTexLevelParameteriv(bind_point, level, gl::TEXTURE_COMPRESSED_IMAGE_SIZE, &mut buffer_size);
                let mut internal_format = mem::uninitialized();
                ctxt.gl.GetTexLevelParameteriv(bind_point, level, gl::TEXTURE_INTERNAL_FORMAT, &mut internal_format);
                
                match ClientFormatAny::from_internal_compressed_format(internal_format as gl::types::GLenum) {
                    Some(known_format) => {
                        let mut buf = Vec::with_capacity(buffer_size as usize);
                        buf.set_len(buffer_size as usize);

                        BufferViewAny::unbind_pixel_pack(&mut ctxt);
                        
                        // adjusting data alignement
                        let ptr = buf.as_ptr() as *const u8;
                        let ptr = ptr as usize;
                        if (ptr % 8) == 0 {
                        } else if (ptr % 4) == 0 && ctxt.state.pixel_store_pack_alignment != 4 {
                            ctxt.state.pixel_store_pack_alignment = 4;
                            ctxt.gl.PixelStorei(gl::PACK_ALIGNMENT, 4);
                        } else if (ptr % 2) == 0 && ctxt.state.pixel_store_pack_alignment > 2 {
                            ctxt.state.pixel_store_pack_alignment = 2;
                            ctxt.gl.PixelStorei(gl::PACK_ALIGNMENT, 2);
                        } else if ctxt.state.pixel_store_pack_alignment != 1 {
                            ctxt.state.pixel_store_pack_alignment = 1;
                            ctxt.gl.PixelStorei(gl::PACK_ALIGNMENT, 1);
                        }

                        ctxt.gl.GetCompressedTexImage(bind_point, level, buf.as_mut_ptr() as *mut _);
                        Some((known_format, buf))
                    },
                    None => None,
                }

            } else {
                None
            }
        }
    }
}

impl TextureAny {
    /// Returns the width of the texture.
    pub fn get_width(&self) -> u32 {
        match self.ty {
            Dimensions::Texture1d { width, .. } => width,
            Dimensions::Texture1dArray { width, .. } => width,
            Dimensions::Texture2d { width, .. } => width,
            Dimensions::Texture2dArray { width, .. } => width,
            Dimensions::Texture2dMultisample { width, .. } => width,
            Dimensions::Texture2dMultisampleArray { width, .. } => width,
            Dimensions::Texture3d { width, .. } => width,
        }
    }

    /// Returns the height of the texture.
    pub fn get_height(&self) -> Option<u32> {
        match self.ty {
            Dimensions::Texture1d { .. } => None,
            Dimensions::Texture1dArray { .. } => None,
            Dimensions::Texture2d { height, .. } => Some(height),
            Dimensions::Texture2dArray { height, .. } => Some(height),
            Dimensions::Texture2dMultisample { height, .. } => Some(height),
            Dimensions::Texture2dMultisampleArray { height, .. } => Some(height),
            Dimensions::Texture3d { height, .. } => Some(height),
        }
    }

    /// Returns the depth of the texture.
    pub fn get_depth(&self) -> Option<u32> {
        match self.ty {
            Dimensions::Texture3d { depth, .. } => Some(depth),
            _ => None
        }
    }

    /// Returns the array size of the texture.
    pub fn get_array_size(&self) -> Option<u32> {
        match self.ty {
            Dimensions::Texture1d { .. } => None,
            Dimensions::Texture1dArray { array_size, .. } => Some(array_size),
            Dimensions::Texture2d { .. } => None,
            Dimensions::Texture2dArray { array_size, .. } => Some(array_size),
            Dimensions::Texture2dMultisample { .. } => None,
            Dimensions::Texture2dMultisampleArray { array_size, .. } => Some(array_size),
            Dimensions::Texture3d { .. } => None,
        }
    }

    /// Returns the number of mipmap levels of the texture.
    pub fn get_mipmap_levels(&self) -> u32 {
        self.levels
    }

    /// Returns the type of the texture (1D, 2D, 3D, etc.).
    pub fn get_texture_type(&self) -> Dimensions {
        self.ty
    }

    /// Determines the internal format of this texture.
    pub fn get_internal_format(&self) -> Result<InternalFormat, GetFormatError> {
        if let Some(format) = self.actual_format.get() {
            format

        } else {
            let mut ctxt = self.context.make_current();
            let format = get_format::get_format(&mut ctxt, self);
            self.actual_format.set(Some(format.clone()));
            format
        }
    }

    /// Returns a structure that represents a specific mipmap of the texture.
    ///
    /// Returns `None` if out of range.
    pub fn mipmap(&self, layer: u32, level: u32) -> Option<TextureAnyMipmap> {
        if layer >= self.get_array_size().unwrap_or(1) {
            return None;
        }

        if level >= self.levels {
            return None;
        }

        let pow = 2u32.pow(level);
        Some(TextureAnyMipmap {
            texture: self,
            level: level,
            layer: layer,
            width: cmp::max(1, self.get_width() / pow),
            height: self.get_height().map(|height| cmp::max(1, height / pow)),
            depth: self.get_depth().map(|depth| cmp::max(1, depth / pow)),
        })
    }
}

impl TextureExt for TextureAny {
    fn get_context(&self) -> &Rc<Context> {
        &self.context
    }

    fn get_bind_point(&self) -> gl::types::GLenum {
        match self.ty {
            Dimensions::Texture1d { .. } => gl::TEXTURE_1D,
            Dimensions::Texture1dArray { .. } => gl::TEXTURE_1D_ARRAY,
            Dimensions::Texture2d { .. } => gl::TEXTURE_2D,
            Dimensions::Texture2dArray { .. } => gl::TEXTURE_2D_ARRAY,
            Dimensions::Texture2dMultisample { .. } => gl::TEXTURE_2D_MULTISAMPLE,
            Dimensions::Texture2dMultisampleArray { .. } => gl::TEXTURE_2D_MULTISAMPLE_ARRAY,
            Dimensions::Texture3d { .. } => gl::TEXTURE_3D,
        }
    }

    fn bind_to_current(&self, ctxt: &mut CommandContext) -> gl::types::GLenum {
        let bind_point = self.get_bind_point();

        let texture_unit = ctxt.state.active_texture;
        if ctxt.state.texture_units[texture_unit as usize].texture != self.id {
            unsafe { ctxt.gl.BindTexture(bind_point, self.id) };
            ctxt.state.texture_units[texture_unit as usize].texture = self.id;
        }

        bind_point
    }
}

impl GlObject for TextureAny {
    type Id = gl::types::GLuint;
    fn get_id(&self) -> gl::types::GLuint {
        self.id
    }
}

impl fmt::Debug for TextureAny {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(fmt, "Texture #{} (dimensions: {}x{}x{}x{})", self.id,
               self.get_width(), self.get_height().unwrap_or(1), self.get_depth().unwrap_or(1),
               self.get_array_size().unwrap_or(1))
    }
}

impl Drop for TextureAny {
    fn drop(&mut self) {
        let mut ctxt = self.context.make_current();

        // removing FBOs which contain this texture
        fbo::FramebuffersContainer::purge_texture(&mut ctxt, self.id);

        // resetting the bindings
        for tex_unit in ctxt.state.texture_units.iter_mut() {
            if tex_unit.texture == self.id {
                tex_unit.texture = 0;
            }
        }

        unsafe { ctxt.gl.DeleteTextures(1, [ self.id ].as_ptr()); }
    }
}
