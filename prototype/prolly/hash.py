"""Hash20 内容寻址哈希。

dolt 对应实现:
  - go/store/hash/hash.go::Hash20        — SHA-512(data) 取前 20 字节 (160 bit)
  - go/store/hash/hash.go::Hash20.String — base32 变体文本编码,
    字母表 "0123456789abcdefghijklmnopqrstuv" (即 0-9 + a-v), 与 dolt/noms 一致。

文本编码为什么选 base32 变体而不是 hex:
  1. 20 字节 = 160 bit = 32 x 5bit, 标准 MSB-first base32 分组恰好零填充,
     编码定长 32 字符 (hex 需 40 字符, 省 20%);
  2. 字母表 ASCII 单调: '0'-'9' 编码值 0-9, 'a'-'v' 编码值 10-31,
     且 ASCII 顺序 == 编码值顺序, 编码又定长
     ⇒ 字符串字典序 == 原始字节序, 满足"排序后文本序=字节序"
     (对象存储按地址前缀列出时天然有序);
  3. 全小写字母数字, 对 URL / 文件路径 / 对象存储 key 友好。
"""
from __future__ import annotations

import hashlib
import random

ALPHABET = "0123456789abcdefghijklmnopqrstuv"
SIZE = 20
_ENCODE_LEN = 32  # 160 bit / 5 bit
_DECODE = {c: i for i, c in enumerate(ALPHABET)}


class Hash20(bytes):
    """20 字节内容地址。字节序继承 bytes; 文本序见 .base32 (与字节序一致)。"""

    __slots__ = ()

    def __new__(cls, data):
        if isinstance(data, memoryview):
            data = data.tobytes()
        if not isinstance(data, (bytes, bytearray)) or len(data) != SIZE:
            got = len(data) if isinstance(data, (bytes, bytearray)) else type(data).__name__
            raise ValueError(f"Hash20 需要 {SIZE} 字节, 得到 {got}")
        return super().__new__(cls, bytes(data))

    @classmethod
    def of(cls, data: bytes) -> "Hash20":
        """addr = sha512(data)[:20] (dolt: hash.FromData)。"""
        return cls(hashlib.sha512(data).digest()[:SIZE])

    @classmethod
    def from_base32(cls, text: str) -> "Hash20":
        if len(text) != _ENCODE_LEN:
            raise ValueError(f"base32 长度应为 {_ENCODE_LEN}: {text!r}")
        n = 0
        for c in text:
            v = _DECODE.get(c)
            if v is None:
                raise ValueError(f"非法 base32 字符 {c!r} in {text!r}")
            n = (n << 5) | v
        return cls(n.to_bytes(SIZE, "big"))

    @property
    def base32(self) -> str:
        n = int.from_bytes(self, "big")
        chars = []
        for _ in range(_ENCODE_LEN):
            n, r = divmod(n, 32)
            chars.append(ALPHABET[r])
        return "".join(reversed(chars))

    def __str__(self) -> str:
        return self.base32

    def __repr__(self) -> str:
        return f"Hash20('{self.base32}')"


# ---------------------------------------------------------------------------
# 单元测试 (pytest 风格, 纯 assert)
# ---------------------------------------------------------------------------

def test_of_is_sha512_prefix():
    raw = hashlib.sha512(b"dendro").digest()[:SIZE]
    h = Hash20.of(b"dendro")
    assert bytes(h) == raw, f"Hash20.of 应为 sha512 前 20 字节: {bytes(h).hex()} != {raw.hex()}"


def test_base32_roundtrip():
    rng = random.Random(20260907)
    for i in range(500):
        raw = rng.randbytes(SIZE)
        h = Hash20(raw)
        text = h.base32
        assert len(text) == _ENCODE_LEN, f"编码长度应 {_ENCODE_LEN}: {text!r}"
        assert all(c in _DECODE for c in text), f"出现字母表外字符: {text!r}"
        assert Hash20.from_base32(text) == h, f"roundtrip 失败 #{i}: {text}"


def test_text_order_equals_byte_order():
    rng = random.Random(42)
    hs = [Hash20(rng.randbytes(SIZE)) for _ in range(400)]
    by_text = sorted(hs, key=lambda h: h.base32)
    by_bytes = sorted(hs)
    assert by_text == by_bytes, "base32 文本排序应等于字节排序"
    for a, b in zip(hs, hs[1:]):
        assert (a < b) == (a.base32 < b.base32), f"序不一致: {a} vs {b}"


def test_rejects_bad_input():
    for bad in (b"", b"x" * 19, b"x" * 21, 123):
        try:
            Hash20(bad)
        except ValueError:
            pass
        else:
            raise AssertionError(f"应拒绝非法输入: {bad!r}")
    try:
        Hash20.from_base32("0" * 31 + "w")  # 'w' 不在字母表
    except ValueError:
        pass
    else:
        raise AssertionError("应拒绝字母表外字符 'w'")


def test_deterministic_and_distinct():
    assert Hash20.of(b"a") == Hash20.of(b"a"), "同内容必须同地址"
    assert Hash20.of(b"a") != Hash20.of(b"b"), "不同内容地址应不同"
    assert len({Hash20.of(bytes([i])).base32 for i in range(256)}) == 256, "256 个单字节输入地址应互异"


TESTS = [
    test_of_is_sha512_prefix,
    test_base32_roundtrip,
    test_text_order_equals_byte_order,
    test_rejects_bad_input,
    test_deterministic_and_distinct,
]

if __name__ == "__main__":
    for t in TESTS:
        t()
        print(f"PASS {t.__name__}")
    print(f"hash.py: {len(TESTS)} tests passed")
