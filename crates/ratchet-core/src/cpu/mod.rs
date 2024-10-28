mod binary;
pub mod gemm;
pub mod rope;
mod unary;
mod utils;

use crate::{
    dequantize, Binary, BinaryOp, CPUBuffer, Cast, Concat, DType, IndexSelect, InvariantError,
    LazyOp, OpGuards, Operation, OperationError, RVec, Shape, Storage, StorageView, Strides,
    Tensor, TensorDType, Unary, UnaryOp,
};
use anyhow::anyhow;
use core::marker::PhantomData;
use half::{bf16, f16};
use num_traits::Float;
use rope::cpu_rope;
use utils::cpu_store_result;

pub fn apply_operation(op: LazyOp, dst: Tensor) -> Result<Tensor, OperationError> {
    match op {
        LazyOp::Binary(b) => b.apply_cpu(dst),
        LazyOp::Cast(c) => cpu_cast(c, dst),
        LazyOp::Matmul(m) => m.apply_cpu(dst),
        LazyOp::Softmax(_s) => todo!(),
        LazyOp::RoPE(r) => cpu_rope(r, dst),
        LazyOp::Unary(u) => u.apply_cpu(dst),
        LazyOp::Reindex(_r) => todo!(),
        LazyOp::Concat(c) => cpu_concat(c, dst),
        LazyOp::Norm(_n) => todo!(),
        LazyOp::Conv(_c) => todo!(),
        LazyOp::Select(i) => cpu_index_select(i, dst),
        LazyOp::IndexWrite(_i) => todo!(),
        LazyOp::Cache(_c) => todo!(),
        LazyOp::Const => todo!(),
        LazyOp::View(_) => todo!(),
    }
}

pub struct StridedIterator<'a> {
    shape: &'a Shape,
    strides: &'a Strides,
    next_index: Option<usize>,
    multi_index: Vec<usize>,
}

impl<'a> StridedIterator<'a> {
    pub fn new(shape: &'a Shape, strides: &'a Strides, start_offset: usize) -> Self {
        Self {
            shape,
            strides,
            next_index: if shape.numel() == 0 {
                None
            } else {
                Some(start_offset)
            },
            multi_index: vec![0; shape.len()],
        }
    }
}

impl<'a> Iterator for StridedIterator<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        let storage_index = match self.next_index {
            None => return None,
            Some(storage_index) => storage_index,
        };
        let mut updated = false;
        let mut next_storage_index = storage_index;
        for ((multi_i, max_i), stride_i) in self
            .multi_index
            .iter_mut()
            .zip(self.shape.iter())
            .zip(self.strides.iter())
            .rev()
        {
            let next_i = *multi_i + 1;
            if next_i < *max_i {
                *multi_i = next_i;
                updated = true;
                next_storage_index += *stride_i as usize;
                break;
            } else {
                next_storage_index -= *multi_i * *stride_i as usize;
                *multi_i = 0
            }
        }
        self.next_index = if updated {
            Some(next_storage_index)
        } else {
            None
        };
        Some(storage_index)
    }
}

impl<'a> From<(&'a Shape, &'a Strides)> for StridedIterator<'a> {
    fn from((shape, strides): (&'a Shape, &'a Strides)) -> Self {
        StridedIterator::new(shape, strides, 0)
    }
}

impl<'a> From<(&'a Shape, &'a Strides, usize)> for StridedIterator<'a> {
    fn from((shape, strides, offset): (&'a Shape, &'a Strides, usize)) -> Self {
        StridedIterator::new(shape, strides, offset)
    }
}

pub trait CPUOperation: Operation {
    fn apply_cpu(&self, dst: Tensor) -> Result<Tensor, OperationError>;
}

fn index_select<T: TensorDType>(
    index_select: IndexSelect,
    dst: Tensor,
) -> Result<Tensor, OperationError> {
    let src = index_select.src();
    let indices = index_select.indices();
    let dim = index_select.dim();

    // TODO: Add support for other indexing types
    if !matches!(indices.dt(), DType::I32) {
        return Err(InvariantError::DTypeMismatch {
            expected: DType::I32,
            actual: indices.dt(),
        }
        .into());
    }

    let mut dst_dims = src.shape().to_vec();
    let indices_dims = indices.shape().to_vec();

    let src_dim = dst_dims[dim];
    let n_ids = indices_dims[0];
    dst_dims[dim] = n_ids;

    let dst_len: usize = dst_dims.iter().product();
    let left_len: usize = dst_dims[..dim].iter().product();
    let right_len: usize = dst_dims[dim + 1..].iter().product();

    let src = src.to_vec::<T>()?;
    let indices = indices.to_vec::<i32>()?;
    let mut result = vec![T::zero(); dst_len];

    for left_i in 0..left_len {
        let start_src_idx = left_i * right_len * src_dim;
        let start_dst_idx = left_i * right_len * n_ids;
        for (i, idx) in indices.iter().enumerate().take(n_ids) {
            let src_idx = start_src_idx + *idx as usize * right_len;
            let dst_idx = start_dst_idx + i * right_len;
            result[dst_idx..dst_idx + right_len]
                .copy_from_slice(&src[src_idx..src_idx + right_len]);
        }
    }
    cpu_store_result(&dst, &result);
    Ok(dst)
}

fn qindex_select(op: IndexSelect, dst: Tensor) -> Result<Tensor, OperationError> {
    // NOTE: qindex_select is functional but not optimized at all.
    // Currently we simply dequantize the entire input tensor to f32 and then call index_select.
    // Because of borrowing rules dequantizing also requires a deep clone of the input tensor, which is less than ideal.
    // In the future we would rather directly index the raw buffer of the quantized tensor and dequantize only what is required.
    // TODO: Add support for direct indexing + partial dequantization
    let src = op.src().deep_clone();

    // NOTE: Support for other quantization types is dependent on the corresponding dequantization functions.
    let src = dequantize(src);
    let indices = op.indices().clone();
    let dim = op.dim();

    index_select::<f32>(IndexSelect::new(src, indices, dim), dst)
}

pub fn cpu_index_select(i: IndexSelect, dst: Tensor) -> Result<Tensor, OperationError> {
    match i.src().dt() {
        DType::F32 => index_select::<f32>(i, dst),
        DType::F16 => index_select::<f16>(i, dst),
        DType::BF16 => index_select::<bf16>(i, dst),
        DType::Q8_0F(_) => qindex_select(i, dst),
        dtype => Err(InvariantError::UnsupportedDType(dtype).into()),
    }
}

fn direct_cast<T: TensorDType, U: TensorDType>(
    input: &Tensor,
    dst: &Tensor,
) -> Result<(), OperationError> {
    let input = input.to_vec::<T>()?;
    let result =
        bytemuck::try_cast_slice::<T, U>(&input).map_err(|_| anyhow!("Failed direct cast"))?;
    cpu_store_result(dst, result);
    Ok(())
}

pub fn cpu_cast(cast: Cast, dst: Tensor) -> Result<Tensor, OperationError> {
    if cast.input().dt() == cast.dst_dt() {
        return Ok(cast.input().clone());
    }
    match (cast.input().dt(), cast.dst_dt()) {
        // F32 ->
        (DType::F32, DType::F16) => unary_apply_fn::<f32, f16>(cast.input(), &dst, f16::from_f32)?,
        (DType::F32, DType::BF16) => {
            unary_apply_fn::<f32, bf16>(cast.input(), &dst, bf16::from_f32)?
        }
        (DType::F32, DType::I32) => direct_cast::<f32, i32>(cast.input(), &dst)?,
        (DType::F32, DType::U32) => direct_cast::<f32, u32>(cast.input(), &dst)?,

        // F16 ->
        (DType::F16, DType::F32) => unary_apply_fn::<f16, f32>(cast.input(), &dst, f32::from)?,

        // BF16 ->
        (DType::BF16, DType::F32) => unary_apply_fn::<bf16, f32>(cast.input(), &dst, f32::from)?,

        // I32 ->
        (DType::I32, DType::F32) => direct_cast::<i32, f32>(cast.input(), &dst)?,

        // U32 ->
        (DType::U32, DType::F32) => direct_cast::<u32, f32>(cast.input(), &dst)?,

        _ => unimplemented!("Cannot cast {:?} -> {:?}", cast.input().dt(), cast.dst_dt()),
    };

    Ok(dst)
}

pub(crate) fn concat<T: TensorDType>(
    inputs: &[(Shape, Vec<T>)],
    dim: usize,
    dst_shape: &Shape,
    dst: &mut [T],
) -> Result<(), OperationError> {
    let dst_dim_len = dst_shape[dim];
    let block: usize = dst_shape.iter().skip(1 + dim).product();
    let dst_s = block * dst_dim_len;
    let src_o = 0;
    let mut dst_o = 0;
    for (src_s, src) in inputs {
        let a_dim: usize = src_s.iter().take(dim).product();
        let b_dim = block * src_s[dim];
        for idx in 0..a_dim {
            let dst_idx = idx * dst_s + dst_o;
            let src_idx = idx * b_dim + src_o;
            let dst_t = &mut dst[dst_idx..dst_idx + b_dim];
            let src = &src[src_idx..src_idx + b_dim];
            dst_t.copy_from_slice(src)
        }
        dst_o += b_dim;
    }
    Ok(())
}
pub(crate) fn apply_concat<T: TensorDType>(
    inputs: RVec<Tensor>,
    dim: usize,
    dst: Tensor,
) -> Result<Tensor, OperationError> {
    let dst_size = dst.shape().numel();
    let mut result = vec![T::zero(); dst_size];

    let inputs = inputs
        .iter()
        .map(|t| match t.to_vec::<T>() {
            Ok(v) => Ok((t.shape().clone(), v)),
            Err(e) => Err(e.into()),
        })
        .collect::<Result<Vec<_>, OperationError>>();

    concat(&inputs?, dim, dst.shape(), &mut result)?;
    cpu_store_result(&dst, &result);
    Ok(dst)
}

pub fn cpu_concat(Concat { inputs, dim }: Concat, dst: Tensor) -> Result<Tensor, OperationError> {
    match dst.dt() {
        DType::F32 => apply_concat::<f32>(inputs, dim, dst),
        DType::F16 => apply_concat::<f16>(inputs, dim, dst),
        DType::BF16 => apply_concat::<bf16>(inputs, dim, dst),
        dtype => Err(InvariantError::UnsupportedDType(dtype).into()),
    }
}

#[inline]
fn unary_apply_fn_helper<T: TensorDType, U: TensorDType>(src: &[T], dst: &mut [U], f: fn(T) -> U) {
    assert_eq!(src.len(), dst.len());
    for (s, d) in src.iter().copied().zip(dst.iter_mut()) {
        *d = f(s);
    }
}

#[inline]
pub fn unary_apply_fn<T: TensorDType, U: TensorDType>(
    input: &Tensor,
    dst: &Tensor,
    f: fn(T) -> U,
) -> Result<(), OperationError> {
    let input = input.to_vec::<T>()?;
    let mut result = vec![U::zero(); dst.shape().numel()];
    unary_apply_fn_helper(&input, &mut result, f);
    cpu_store_result(dst, &result);
    Ok(())
}

#[inline]
fn binary_apply_fn_helper<T: TensorDType, U: TensorDType>(
    lhs: &[T],
    rhs: &[T],
    dst: &mut [U],
    f: fn(T, T) -> U,
) {
    assert_eq!(lhs.len(), dst.len());
    assert_eq!(rhs.len(), dst.len());
    for ((l, r), d) in lhs
        .iter()
        .copied()
        .zip(rhs.iter().copied())
        .zip(dst.iter_mut())
    {
        *d = f(l, r);
    }
}

#[inline]
fn binary_apply_inplace_helper<T: TensorDType>(lhs: &mut [T], rhs: &[T], f: fn(T, T) -> T) {
    assert_eq!(lhs.len(), rhs.len());
    lhs.iter_mut().zip(rhs.iter()).for_each(|(l, r)| {
        *l = f(*l, *r);
    });
}

#[inline]
pub fn binary_apply_fn<T: TensorDType, U: TensorDType>(
    lhs: &Tensor,
    rhs: &Tensor,
    dst: &Tensor,
    f: fn(T, T) -> U,
) -> Result<(), OperationError> {
    let lhs = lhs.to_vec::<T>()?;
    let rhs = rhs.to_vec::<T>()?;
    let mut result = vec![U::zero(); dst.shape().numel()];
    binary_apply_fn_helper(&lhs, &rhs, &mut result, f);
    cpu_store_result(dst, &result);
    Ok(())
}

#[inline]
pub fn binary_apply_inplace<T: TensorDType>(
    lhs: &Tensor,
    rhs: &Tensor,
    dst: &Tensor,
    f: fn(T, T) -> T,
) -> Result<(), OperationError> {
    let mut lhs = lhs.to_vec::<T>()?;
    let rhs = rhs.to_vec::<T>()?;
    binary_apply_inplace_helper(&mut lhs, &rhs, f);
    cpu_store_result(dst, &lhs);
    Ok(())
}
