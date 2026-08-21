# Vendored from reyn-research/model_bundle.py
# Source SHA-256: e2719405fde3d82cb5df3084d229196a9e1ce466c40895057543ffb97ddd2dfc
# Vendored 2026-07-30 because pinned research revision 0333b13 lacks this loader.
# Local portability changes are documented in docs/MODEL_BUNDLE_PROVENANCE.md.

"""Safe, deterministic inference bundles for Reyn Studio.

The production loader in this module never deserializes Python pickle data.
Bundles are deterministic ZIP_STORED archives containing exactly:

* ``manifest.json`` -- canonical JSON metadata and tensor/file inventories.
* ``weights.safetensors`` -- tensor-only model state.

SHA-256 values provide integrity and identity only. Production authenticity
requires fresh threshold-signed TUF metadata from a pinned root plus a detached
``.reynmodel.sig`` Ed25519 signature: both are verified before the ZIP or
Safetensors payload is opened.
"""

from __future__ import annotations

import base64
import binascii
import hashlib
import json
import math
import os
import re
import shutil
import stat
import tempfile
import time
import zipfile
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path, PurePosixPath
from typing import Mapping
from urllib.parse import unquote, urlsplit

if os.name == "nt":
    import msvcrt
else:
    import fcntl

import torch
from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import (
    Ed25519PrivateKey,
    Ed25519PublicKey,
)
from safetensors import safe_open
from safetensors.torch import save as save_safetensors
from tuf.api import exceptions as tuf_exceptions
from tuf.api.metadata import Metadata, Root, Snapshot, Targets, Timestamp
from tuf.ngclient import Updater
from tuf.ngclient.config import UpdaterConfig
from tuf.ngclient.fetcher import FetcherInterface

from pinned_model_trust import PINNED_TUF_ROOT_JSON

BUNDLE_SCHEMA = "com.reyn.inference-model-bundle/1"
BUNDLE_EXTENSION = ".reynmodel"
MANIFEST_NAME = "manifest.json"
WEIGHTS_NAME = "weights.safetensors"
ARCHITECTURE_2D = "reyn.direct-flow-map.2d/1"
ARCHITECTURE_3D = "reyn.direct-flow-map.3d/1"
SIGNATURE_DOCUMENT_SCHEMA = "com.reyn.inference-model-signature/1"
SIGNATURE_PAYLOAD_SCHEMA = "com.reyn.inference-model-signature-payload/1"
SIGNATURE_ALGORITHM = "ed25519"
SIGNATURE_SUFFIX = ".sig"
SIGNATURE_DOMAIN = b"Reyn inference model signature v1\x00"
TUF_TARGET_CUSTOM_SCHEMA = "com.reyn.tuf-model-target/1"
TUF_TRUSTED_STATE_SCHEMA = "com.reyn.tuf-trusted-state/1"
TUF_CURRENT_SCHEMA = "com.reyn.tuf-current/1"
TUF_REPOSITORY_SUFFIX = ".tuf"
TUF_MODELS_ROLE = "models"

MAX_BUNDLE_BYTES = 2 * 1024**3
MAX_MANIFEST_BYTES = 256 * 1024
MAX_WEIGHTS_BYTES = 2 * 1024**3
MAX_SAFETENSORS_HEADER_BYTES = 16 * 1024**2
MAX_TENSORS = 4096
MAX_TENSOR_RANK = 8
MAX_TENSOR_ELEMENTS = 1_000_000_000
MAX_SIGNATURE_BYTES = 64 * 1024
MAX_SIGNATURE_LIFETIME = timedelta(days=366)
MAX_CLOCK_SKEW = timedelta(minutes=5)
MAX_TUF_ROOT_BYTES = 256 * 1024
MAX_TUF_TIMESTAMP_BYTES = 64 * 1024
MAX_TUF_SNAPSHOT_BYTES = 512 * 1024
MAX_TUF_TARGETS_BYTES = 1024 * 1024
MAX_TUF_ROOT_ROTATIONS = 32
MAX_TUF_DELEGATIONS = 8
MAX_TUF_TRACKED_MODELS = 1024
MAX_TUF_METADATA_FILES = 64
MAX_TUF_REPOSITORY_BYTES = 16 * 1024**2
TUF_MIN_ROOT_THRESHOLD = 2
TUF_MIN_TARGETS_THRESHOLD = 2

_ARCHITECTURE_KEYS = frozenset(("id", "config"))
_TOP_LEVEL_KEYS = frozenset(
    (
        "schema",
        "model",
        "architecture",
        "io_schema",
        "normalization",
        "conditioning",
        "support_envelope",
        "source_training",
        "limitations",
        "benchmark_reports",
        "tensor_schema",
        "files",
    )
)
_SAFE_NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")
_SHA256 = re.compile(r"^[0-9a-f]{64}$")
_TORCH_DTYPE_NAMES = {
    torch.float16: "float16",
    torch.bfloat16: "bfloat16",
    torch.float32: "float32",
    torch.float64: "float64",
}
_SAFE_DTYPE_NAMES = {
    "F16": "float16",
    "BF16": "bfloat16",
    "F32": "float32",
    "F64": "float64",
}


class ModelBundleError(ValueError):
    """Actionable validation failure with a stable machine-readable code."""

    def __init__(self, code: str, field: str, message: str):
        self.code = code
        self.field = field
        self.message = message
        super().__init__(f"{code} [{field}]: {message}")


@dataclass(frozen=True)
class LoadedModelBundle:
    manifest: dict
    model: torch.nn.Module
    authenticity: dict


def _fail(code: str, field: str, message: str):
    raise ModelBundleError(code, field, message)


def _exact_keys(value, expected, field):
    if not isinstance(value, dict):
        _fail("bundle.manifest_type", field, "must be an object")
    actual = set(value)
    missing = sorted(set(expected) - actual)
    extra = sorted(actual - set(expected))
    if missing or extra:
        details = []
        if missing:
            details.append(f"missing {missing}")
        if extra:
            details.append(f"unexpected {extra}")
        _fail("bundle.manifest_keys", field, "; ".join(details))


def _finite_number(value, field, *, positive=False, nonnegative=False):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        _fail("bundle.manifest_type", field, "must be a finite number")
    number = float(value)
    if not math.isfinite(number):
        _fail("bundle.nonfinite_metadata", field, "must be finite")
    if positive and number <= 0.0:
        _fail("bundle.manifest_range", field, "must be positive")
    if nonnegative and number < 0.0:
        _fail("bundle.manifest_range", field, "must be nonnegative")
    return number


def _bounded_int(value, field, *, minimum=0, maximum):
    if isinstance(value, bool) or not isinstance(value, int):
        _fail("bundle.manifest_type", field, "must be an integer")
    if not minimum <= value <= maximum:
        _fail(
            "bundle.manifest_range",
            field,
            f"must be between {minimum} and {maximum}",
        )
    return value


def _canonical_json_bytes(value) -> bytes:
    try:
        encoded = json.dumps(
            value,
            ensure_ascii=False,
            allow_nan=False,
            sort_keys=True,
            separators=(",", ":"),
        )
    except (TypeError, ValueError) as exc:
        _fail("bundle.manifest_json", MANIFEST_NAME, f"cannot canonicalize: {exc}")
    return encoded.encode("utf-8") + b"\n"


def _signature_path(path: Path) -> Path:
    return path.with_name(path.name + SIGNATURE_SUFFIX)


def _signature_exact_keys(value, expected, field):
    if not isinstance(value, dict):
        _fail("signature.document_type", field, "must be an object")
    actual = set(value)
    missing = sorted(set(expected) - actual)
    extra = sorted(actual - set(expected))
    if missing or extra:
        details = []
        if missing:
            details.append(f"missing {missing}")
        if extra:
            details.append(f"unexpected {extra}")
        _fail("signature.document_keys", field, "; ".join(details))


def _tuf_exact_keys(value, expected, field):
    if not isinstance(value, dict):
        _fail("tuf.metadata_type", field, "must be an object")
    actual = set(value)
    missing = sorted(set(expected) - actual)
    extra = sorted(actual - set(expected))
    if missing or extra:
        details = []
        if missing:
            details.append(f"missing {missing}")
        if extra:
            details.append(f"unexpected {extra}")
        _fail("tuf.metadata_keys", field, "; ".join(details))


def _parse_utc_timestamp(value, field) -> datetime:
    if not isinstance(value, str) or not re.fullmatch(
        r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", value
    ):
        _fail(
            "signature.timestamp",
            field,
            "must use canonical UTC form YYYY-MM-DDTHH:MM:SSZ",
        )
    try:
        parsed = datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=timezone.utc
        )
    except ValueError as exc:
        _fail("signature.timestamp", field, f"invalid UTC timestamp: {exc}")
    if parsed.strftime("%Y-%m-%dT%H:%M:%SZ") != value:
        _fail(
            "signature.timestamp",
            field,
            "must use canonical UTC form YYYY-MM-DDTHH:MM:SSZ",
        )
    return parsed


def _decode_canonical_base64(value, field, *, expected_bytes):
    if not isinstance(value, str):
        _fail("signature.malformed_base64", field, "must be canonical base64 text")
    try:
        decoded = base64.b64decode(value, validate=True)
    except (binascii.Error, ValueError) as exc:
        _fail("signature.malformed_base64", field, f"invalid base64: {exc}")
    if base64.b64encode(decoded).decode("ascii") != value:
        _fail(
            "signature.malformed_base64",
            field,
            "must use canonical padded base64",
        )
    if len(decoded) != expected_bytes:
        _fail(
            "signature.malformed_base64",
            field,
            f"must decode to exactly {expected_bytes} bytes",
        )
    return decoded


def _parse_signature_document(raw: bytes):
    if not 0 < len(raw) <= MAX_SIGNATURE_BYTES:
        _fail(
            "signature.size_limit",
            "signature",
            f"signature document size must be 1..{MAX_SIGNATURE_BYTES} bytes",
        )

    def reject_duplicate_keys(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                _fail(
                    "signature.duplicate_key",
                    "signature",
                    f"duplicate JSON key {key!r}",
                )
            result[key] = value
        return result

    def reject_json_constant(value):
        _fail(
            "signature.nonfinite_json",
            "signature",
            f"JSON constant {value!r} is not permitted",
        )

    try:
        document = json.loads(
            raw.decode("utf-8"),
            parse_constant=reject_json_constant,
            object_pairs_hook=reject_duplicate_keys,
        )
    except UnicodeDecodeError as exc:
        _fail("signature.encoding", "signature", f"must be UTF-8: {exc}")
    except json.JSONDecodeError as exc:
        _fail("signature.invalid_json", "signature", f"invalid JSON: {exc}")
    if raw != _canonical_json_bytes(document):
        _fail(
            "signature.not_canonical",
            "signature",
            "signature document must use Reyn canonical JSON encoding",
        )
    _signature_exact_keys(document, ("schema", "signed", "signature"), "signature")
    if document["schema"] != SIGNATURE_DOCUMENT_SCHEMA:
        _fail(
            "signature.unsupported_schema",
            "signature.schema",
            f"expected {SIGNATURE_DOCUMENT_SCHEMA!r}",
        )
    payload = document["signed"]
    _signature_exact_keys(
        payload,
        (
            "schema",
            "algorithm",
            "key_id",
            "bundle_schema",
            "bundle_sha256",
            "model",
            "release_sequence",
            "issued_at",
            "expires_at",
        ),
        "signature.signed",
    )
    if payload["schema"] != SIGNATURE_PAYLOAD_SCHEMA:
        _fail(
            "signature.unsupported_payload_schema",
            "signature.signed.schema",
            f"expected {SIGNATURE_PAYLOAD_SCHEMA!r}",
        )
    if payload["algorithm"] != SIGNATURE_ALGORITHM:
        _fail(
            "signature.unsupported_algorithm",
            "signature.signed.algorithm",
            f"expected {SIGNATURE_ALGORITHM!r}",
        )
    key_id = payload["key_id"]
    if not isinstance(key_id, str) or not _SAFE_NAME.fullmatch(key_id):
        _fail(
            "signature.key_id",
            "signature.signed.key_id",
            "must match [A-Za-z0-9][A-Za-z0-9._-]{0,127}",
        )
    if payload["bundle_schema"] != BUNDLE_SCHEMA:
        _fail(
            "signature.bundle_schema",
            "signature.signed.bundle_schema",
            f"expected {BUNDLE_SCHEMA!r}",
        )
    digest = payload["bundle_sha256"]
    if not isinstance(digest, str) or not _SHA256.fullmatch(digest):
        _fail(
            "signature.bundle_digest",
            "signature.signed.bundle_sha256",
            "must be a lowercase SHA-256 digest",
        )
    model = payload["model"]
    _signature_exact_keys(model, ("id", "version"), "signature.signed.model")
    for name in ("id", "version"):
        if not isinstance(model[name], str) or not _SAFE_NAME.fullmatch(model[name]):
            _fail(
                "signature.model_identity",
                f"signature.signed.model.{name}",
                "must match [A-Za-z0-9][A-Za-z0-9._-]{0,127}",
            )
    release_sequence = payload["release_sequence"]
    if (
        isinstance(release_sequence, bool)
        or not isinstance(release_sequence, int)
        or not 0 <= release_sequence <= 2**63 - 1
    ):
        _fail(
            "signature.release_sequence",
            "signature.signed.release_sequence",
            "must be an integer between 0 and 2^63-1",
        )
    issued_at = _parse_utc_timestamp(
        payload["issued_at"], "signature.signed.issued_at"
    )
    expires_at = _parse_utc_timestamp(
        payload["expires_at"], "signature.signed.expires_at"
    )
    if expires_at <= issued_at:
        _fail(
            "signature.validity_window",
            "signature.signed.expires_at",
            "must be later than issued_at",
        )
    if expires_at - issued_at > MAX_SIGNATURE_LIFETIME:
        _fail(
            "signature.validity_window",
            "signature.signed.expires_at",
            f"signature lifetime must not exceed {MAX_SIGNATURE_LIFETIME.days} days",
        )
    signature_bytes = _decode_canonical_base64(
        document["signature"], "signature.signature", expected_bytes=64
    )
    return document, signature_bytes, issued_at, expires_at


class _OfflineMetadataFetcher(FetcherInterface):
    """Strict local-only metadata transport for python-tuf."""

    def __init__(self, metadata_dir: Path):
        self.metadata_dir = metadata_dir.resolve()

    def _fetch(self, url: str):
        parsed = urlsplit(url)
        if (
            parsed.scheme != "https"
            or parsed.netloc != "offline.reyn.invalid"
            or parsed.query
            or parsed.fragment
        ):
            raise tuf_exceptions.DownloadError(
                "offline model verification refuses non-local metadata transport"
            )
        name = unquote(PurePosixPath(parsed.path).name)
        if not _is_tuf_metadata_name(name):
            raise tuf_exceptions.DownloadError(
                f"unsupported offline metadata name {name!r}"
            )
        candidate = self.metadata_dir / name
        if not candidate.is_file() or candidate.is_symlink():
            raise tuf_exceptions.DownloadHTTPError(
                f"offline metadata not found: {name}", 404
            )
        size = candidate.stat().st_size
        if not 0 < size <= MAX_TUF_TARGETS_BYTES:
            raise tuf_exceptions.DownloadLengthMismatchError(
                f"offline metadata {name} has invalid size {size}"
            )
        return iter((candidate.read_bytes(),))


class _PortableOfflineUpdater(Updater):
    """Use a regular root pointer on Windows, where symlinks need extra privilege."""

    def _update_root_symlink(self):
        if os.name != "nt":
            return super()._update_root_symlink()
        self._update_root_file()

    def _update_root_file(self):
        version = self._trusted_set.root.version
        root_history = Path(self._dir) / "root_history" / f"{version}.root.json"
        self._persist_file(
            str(Path(self._dir) / "root.json"),
            root_history.read_bytes(),
        )


def _tuf_repository_path(path: Path) -> Path:
    return path.with_name(path.name + TUF_REPOSITORY_SUFFIX)


def _is_tuf_metadata_name(name: str) -> bool:
    return bool(
        re.fullmatch(
            r"(?:[1-9][0-9]*\.)?(?:root|snapshot|targets|models)\.json"
            r"|timestamp\.json",
            name,
        )
    )


def _validate_offline_metadata_directory(metadata_dir: Path):
    try:
        entries = list(metadata_dir.iterdir())
    except OSError as exc:
        _fail(
            "tuf.repository_invalid",
            "tuf.repository",
            f"could not enumerate offline metadata: {exc}",
        )
    if not 1 <= len(entries) <= MAX_TUF_METADATA_FILES:
        _fail(
            "tuf.repository_size",
            "tuf.repository",
            f"must contain 1..{MAX_TUF_METADATA_FILES} metadata files",
        )
    total = 0
    for entry in entries:
        if (
            entry.is_symlink()
            or not entry.is_file()
            or not _is_tuf_metadata_name(entry.name)
        ):
            _fail(
                "tuf.repository_invalid",
                "tuf.repository",
                f"unsupported metadata entry {entry.name!r}",
            )
        size = entry.stat().st_size
        if not 0 < size <= MAX_TUF_TARGETS_BYTES:
            _fail(
                "tuf.repository_size",
                f"tuf.repository.{entry.name}",
                f"metadata size must be 1..{MAX_TUF_TARGETS_BYTES} bytes",
            )
        total += size
        if total > MAX_TUF_REPOSITORY_BYTES:
            _fail(
                "tuf.repository_size",
                "tuf.repository",
                f"metadata exceeds {MAX_TUF_REPOSITORY_BYTES} total bytes",
            )


def _read_bounded(path: Path, maximum: int, code: str, field: str) -> bytes:
    try:
        size = path.stat().st_size
        if not 0 < size <= maximum:
            _fail(code, field, f"size must be 1..{maximum} bytes; found {size}")
        return path.read_bytes()
    except ModelBundleError:
        raise
    except OSError as exc:
        _fail(code, field, f"could not read {path.name}: {exc}")


def _parse_canonical_state(raw: bytes, *, field: str, maximum: int) -> dict:
    if not 0 < len(raw) <= maximum:
        _fail(
            "tuf.trusted_state_invalid",
            field,
            f"state size must be 1..{maximum} bytes",
        )

    def reject_duplicates(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                _fail(
                    "tuf.trusted_state_invalid",
                    field,
                    f"duplicate key {key!r}",
                )
            result[key] = value
        return result

    try:
        value = json.loads(
            raw.decode("utf-8"),
            object_pairs_hook=reject_duplicates,
            parse_constant=lambda constant: _fail(
                "tuf.trusted_state_invalid",
                field,
                f"JSON constant {constant!r} is not permitted",
            ),
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        _fail("tuf.trusted_state_invalid", field, f"invalid JSON: {exc}")
    if not isinstance(value, dict) or raw != _canonical_json_bytes(value):
        _fail(
            "tuf.trusted_state_invalid",
            field,
            "must be a canonical JSON object",
        )
    return value


def _validate_root_policy(metadata, field):
    if not isinstance(metadata.signed, Root):
        _fail("tuf.root_invalid", field, "metadata must have root type")
    try:
        metadata.verify_delegate("root", metadata)
    except tuf_exceptions.UnsignedMetadataError as exc:
        _fail("tuf.threshold", field, f"root threshold verification failed: {exc}")
    root = metadata.signed
    if not root.consistent_snapshot:
        _fail(
            "tuf.root_policy",
            f"{field}.consistent_snapshot",
            "production roots must enable consistent snapshots",
        )
    for role_name, minimum in (
        ("root", TUF_MIN_ROOT_THRESHOLD),
        ("targets", TUF_MIN_TARGETS_THRESHOLD),
    ):
        role = root.roles.get(role_name)
        if role is None or role.threshold < minimum:
            _fail(
                "tuf.root_policy",
                f"{field}.roles.{role_name}",
                f"requires signature threshold >= {minimum}",
            )
        if role.threshold > len(role.keyids):
            _fail(
                "tuf.root_policy",
                f"{field}.roles.{role_name}",
                "threshold exceeds authorized key count",
            )
        for key_id in role.keyids:
            key = root.keys.get(key_id)
            if (
                key is None
                or key.keytype != "ed25519"
                or key.scheme != "ed25519"
            ):
                _fail(
                    "tuf.root_policy",
                    f"{field}.keys.{key_id}",
                    "root and targets roles require Ed25519 keys",
                )


def _validated_pinned_root() -> bytes:
    raw = PINNED_TUF_ROOT_JSON
    if raw is None:
        _fail(
            "tuf.root_not_pinned",
            "tuf.root",
            "production model loading requires reviewed root metadata pinned in source",
        )
    if not isinstance(raw, bytes) or not 0 < len(raw) <= MAX_TUF_ROOT_BYTES:
        _fail(
            "tuf.root_invalid",
            "tuf.root",
            f"pinned root must be 1..{MAX_TUF_ROOT_BYTES} bytes",
        )
    try:
        metadata = Metadata.from_bytes(raw)
    except (ValueError, TypeError) as exc:
        _fail("tuf.root_invalid", "tuf.root", f"invalid pinned root metadata: {exc}")
    _validate_root_policy(metadata, "tuf.root")
    return raw


def _trusted_state_default(pinned_root_sha256: str) -> dict:
    return {
        "schema": TUF_TRUSTED_STATE_SCHEMA,
        "pinned_root_sha256": pinned_root_sha256,
        "metadata_versions": {},
        "models": {},
    }


def _load_trusted_state(
    state_root: Path,
    working_metadata: Path,
    pinned_root_sha256: str,
):
    pointer_path = state_root / "current.json"
    if not pointer_path.exists():
        working_metadata.mkdir(parents=True)
        return False, _trusted_state_default(pinned_root_sha256)
    pointer = _parse_canonical_state(
        _read_bounded(
            pointer_path,
            MAX_SIGNATURE_BYTES,
            "tuf.trusted_state_invalid",
            "tuf.current",
        ),
        field="tuf.current",
        maximum=MAX_SIGNATURE_BYTES,
    )
    _tuf_exact_keys(pointer, ("schema", "generation"), "tuf.current")
    generation = pointer["generation"]
    if pointer["schema"] != TUF_CURRENT_SCHEMA or not isinstance(
        generation, str
    ) or not _SHA256.fullmatch(generation):
        _fail(
            "tuf.trusted_state_invalid",
            "tuf.current",
            "invalid trusted-state generation pointer",
        )
    generation_dir = state_root / "generations" / generation
    metadata_dir = generation_dir / "metadata"
    if not metadata_dir.is_dir() or metadata_dir.is_symlink():
        _fail(
            "tuf.trusted_state_invalid",
            "tuf.current",
            "trusted-state generation is missing",
        )
    shutil.copytree(metadata_dir, working_metadata)
    state = _parse_canonical_state(
        _read_bounded(
            generation_dir / "state.json",
            MAX_TUF_TARGETS_BYTES,
            "tuf.trusted_state_invalid",
            "tuf.state",
        ),
        field="tuf.state",
        maximum=MAX_TUF_TARGETS_BYTES,
    )
    _tuf_exact_keys(
        state,
        ("schema", "pinned_root_sha256", "metadata_versions", "models"),
        "tuf.state",
    )
    if state["schema"] != TUF_TRUSTED_STATE_SCHEMA:
        _fail(
            "tuf.trusted_state_invalid",
            "tuf.state.schema",
            "unsupported trusted-state schema",
        )
    if state["pinned_root_sha256"] != pinned_root_sha256:
        _fail(
            "tuf.root_pin_changed",
            "tuf.state.pinned_root_sha256",
            "trusted state was initialized from a different pinned root",
        )
    if not isinstance(state["metadata_versions"], dict) or not isinstance(
        state["models"], dict
    ):
        _fail(
            "tuf.trusted_state_invalid",
            "tuf.state",
            "metadata_versions and models must be objects",
        )
    for role_name, version in state["metadata_versions"].items():
        if (
            role_name not in ("root", "timestamp", "snapshot", "targets", "models")
            or isinstance(version, bool)
            or not isinstance(version, int)
            or version < 1
        ):
            _fail(
                "tuf.trusted_state_invalid",
                f"tuf.state.metadata_versions.{role_name}",
                "contains an invalid role or version",
            )
    if len(state["models"]) > MAX_TUF_TRACKED_MODELS:
        _fail(
            "tuf.trusted_state_invalid",
            "tuf.state.models",
            f"tracks more than {MAX_TUF_TRACKED_MODELS} models",
        )
    for model_id, model_state in state["models"].items():
        if not isinstance(model_id, str) or not _SAFE_NAME.fullmatch(model_id):
            _fail(
                "tuf.trusted_state_invalid",
                "tuf.state.models",
                "contains an invalid model ID",
            )
        _tuf_exact_keys(
            model_state,
            ("release_sequence", "bundle_sha256", "model_version"),
            f"tuf.state.models.{model_id}",
        )
        if (
            isinstance(model_state["release_sequence"], bool)
            or not isinstance(model_state["release_sequence"], int)
            or model_state["release_sequence"] < 0
            or not isinstance(model_state["bundle_sha256"], str)
            or not _SHA256.fullmatch(model_state["bundle_sha256"])
            or not isinstance(model_state["model_version"], str)
            or not _SAFE_NAME.fullmatch(model_state["model_version"])
        ):
            _fail(
                "tuf.trusted_state_invalid",
                f"tuf.state.models.{model_id}",
                "contains invalid rollback state",
            )
    return True, state


def _metadata_version(path: Path, expected_type):
    metadata = Metadata.from_file(str(path))
    if not isinstance(metadata.signed, expected_type):
        _fail(
            "tuf.metadata_type",
            f"tuf.{path.name}",
            f"expected {expected_type.type} metadata",
        )
    return metadata


def _require_meta_hashes(meta, field):
    if (
        meta.length is None
        or not isinstance(meta.hashes, dict)
        or not _SHA256.fullmatch(meta.hashes.get("sha256", ""))
    ):
        _fail(
            "tuf.metadata_binding",
            field,
            "metadata must declare exact length and SHA-256",
        )


def _raise_tuf_error(exc):
    if isinstance(exc, tuf_exceptions.ExpiredMetadataError):
        _fail("tuf.expired_metadata", "tuf.metadata", str(exc))
    if isinstance(
        exc,
        (
            tuf_exceptions.BadVersionNumberError,
            tuf_exceptions.EqualVersionNumberError,
        ),
    ):
        _fail("tuf.rollback", "tuf.metadata", str(exc))
    if isinstance(exc, tuf_exceptions.UnsignedMetadataError):
        _fail("tuf.threshold", "tuf.metadata", str(exc))
    if isinstance(exc, tuf_exceptions.LengthOrHashMismatchError):
        _fail("tuf.mix_and_match", "tuf.metadata", str(exc))
    if isinstance(exc, tuf_exceptions.DownloadLengthMismatchError):
        _fail("tuf.metadata_size", "tuf.metadata", str(exc))
    if isinstance(exc, tuf_exceptions.DownloadError):
        _fail("tuf.metadata_unavailable", "tuf.metadata", str(exc))
    _fail("tuf.invalid_metadata", "tuf.metadata", str(exc))


def _target_custom(target_info, target_path):
    custom = target_info.custom
    if not isinstance(custom, dict):
        _fail(
            "tuf.target_custom",
            f"tuf.targets.{target_path}.custom",
            "model target requires Reyn custom metadata",
        )
    _tuf_exact_keys(
        custom,
        (
            "schema",
            "model",
            "release_sequence",
            "detached_signature",
            "revoked_signature_key_ids",
        ),
        f"tuf.targets.{target_path}.custom",
    )
    if custom["schema"] != TUF_TARGET_CUSTOM_SCHEMA:
        _fail(
            "tuf.target_custom",
            f"tuf.targets.{target_path}.custom.schema",
            f"expected {TUF_TARGET_CUSTOM_SCHEMA!r}",
        )
    model = custom["model"]
    _tuf_exact_keys(model, ("id", "version"), "tuf.target.custom.model")
    sequence = custom["release_sequence"]
    if (
        isinstance(sequence, bool)
        or not isinstance(sequence, int)
        or not 0 <= sequence <= 2**63 - 1
    ):
        _fail(
            "tuf.target_custom",
            "tuf.target.custom.release_sequence",
            "must be an integer between 0 and 2^63-1",
        )
    signature = custom["detached_signature"]
    _tuf_exact_keys(
        signature,
        ("target_path", "algorithm", "key_id", "public_key"),
        "tuf.target.custom.detached_signature",
    )
    revoked = custom["revoked_signature_key_ids"]
    if (
        not isinstance(revoked, list)
        or revoked != sorted(set(revoked))
        or any(not isinstance(value, str) or not _SAFE_NAME.fullmatch(value) for value in revoked)
    ):
        _fail(
            "tuf.target_custom",
            "tuf.target.custom.revoked_signature_key_ids",
            "must be a sorted unique list of key IDs",
        )
    if signature["algorithm"] != SIGNATURE_ALGORITHM:
        _fail(
            "tuf.target_custom",
            "tuf.target.custom.detached_signature.algorithm",
            f"must be {SIGNATURE_ALGORITHM!r}",
        )
    public_key = _decode_canonical_base64(
        signature["public_key"],
        "tuf.target.custom.detached_signature.public_key",
        expected_bytes=32,
    )
    return custom, public_key


def _trusted_state_publish_hook():
    """Test seam immediately before the atomic current-pointer update."""


def _fsync_directory(path: Path, *, platform_name: str | None = None):
    if (os.name if platform_name is None else platform_name) == "nt":
        return
    descriptor = os.open(path, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def _fsync_regular_file(path: Path, *, platform_name: str | None = None):
    platform_name = os.name if platform_name is None else platform_name
    mode = "r+b" if platform_name == "nt" else "rb"
    with path.open(mode) as stream:
        os.fsync(stream.fileno())


def _atomic_write(path: Path, raw: bytes):
    temporary_path = None
    try:
        with tempfile.NamedTemporaryFile(
            prefix=f".{path.name}.",
            suffix=".tmp",
            dir=path.parent,
            delete=False,
        ) as temporary:
            temporary_path = Path(temporary.name)
            temporary.write(raw)
            temporary.flush()
            os.fsync(temporary.fileno())
        os.chmod(temporary_path, 0o600)
        os.replace(temporary_path, path)
        temporary_path = None
        _fsync_directory(path.parent)
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)


def _publish_trusted_state(state_root: Path, working_root: Path, state: dict):
    state_raw = _canonical_json_bytes(state)
    (working_root / "state.json").write_bytes(state_raw)
    os.chmod(working_root / "state.json", 0o600)
    for candidate in sorted(working_root.rglob("*")):
        if candidate.is_file():
            _fsync_regular_file(candidate)
    _fsync_directory(working_root / "metadata")
    _fsync_directory(working_root)

    digest = hashlib.sha256()
    for candidate in sorted(working_root.rglob("*")):
        if candidate.is_file():
            digest.update(candidate.relative_to(working_root).as_posix().encode("utf-8"))
            digest.update(b"\x00")
            digest.update(candidate.read_bytes())
    generation = digest.hexdigest()
    generations = state_root / "generations"
    generations.mkdir(mode=0o700, exist_ok=True)
    destination = generations / generation
    created = False
    if destination.exists():
        shutil.rmtree(working_root)
    else:
        os.replace(working_root, destination)
        created = True
        _fsync_directory(generations)
    try:
        _trusted_state_publish_hook()
        pointer = {
            "schema": TUF_CURRENT_SCHEMA,
            "generation": generation,
        }
        _atomic_write(state_root / "current.json", _canonical_json_bytes(pointer))
        for obsolete in generations.iterdir():
            if (
                obsolete.name != generation
                and obsolete.is_dir()
                and not obsolete.is_symlink()
            ):
                shutil.rmtree(obsolete)
        _fsync_directory(generations)
    except Exception:
        if created:
            shutil.rmtree(destination, ignore_errors=True)
        raise


@contextmanager
def _locked_trusted_state(trusted_state_dir):
    if trusted_state_dir is None:
        _fail(
            "tuf.trusted_state_required",
            "trusted_state_dir",
            "production verification requires a persistent trusted-state directory",
        )
    state_root = Path(os.path.abspath(Path(trusted_state_dir).expanduser()))
    if state_root.exists() and state_root.is_symlink():
        _fail(
            "tuf.trusted_state_invalid",
            "trusted_state_dir",
            "trusted-state directory must not be a symbolic link",
        )
    state_root.mkdir(parents=True, exist_ok=True, mode=0o700)
    lock_path = state_root / ".lock"
    with lock_path.open("a+b") as lock:
        os.chmod(lock_path, 0o600)
        if os.name == "nt":
            lock.seek(0, os.SEEK_END)
            if lock.tell() == 0:
                lock.write(b"\0")
                lock.flush()
                os.fsync(lock.fileno())
            deadline = time.monotonic() + 30.0
            while True:
                lock.seek(0)
                try:
                    msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
                    break
                except OSError:
                    if time.monotonic() >= deadline:
                        _fail(
                            "tuf.trusted_state_locked",
                            "trusted_state_dir",
                            "could not acquire the trusted-state lock within 30 seconds",
                        )
                    time.sleep(0.05)
        else:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        try:
            yield state_root
        finally:
            if os.name == "nt":
                lock.seek(0)
                msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(lock.fileno(), fcntl.LOCK_UN)


def _prepare_tuf_authentication(
    path: Path,
    signature_path: Path,
    payload: dict,
    bundle_sha256: str,
    state_root: Path,
):
    repository = _tuf_repository_path(path)
    metadata_source = repository / "metadata"
    if (
        not metadata_source.is_dir()
        or metadata_source.is_symlink()
        or repository.is_symlink()
    ):
        _fail(
            "tuf.repository_missing",
            "tuf.repository",
            f"required offline metadata directory not found: {metadata_source}",
        )
    _validate_offline_metadata_directory(metadata_source)
    working_root = Path(
        tempfile.mkdtemp(prefix=".tuf-staging-", dir=state_root)
    )
    working_metadata = working_root / "metadata"
    try:
        pinned_root = _validated_pinned_root()
        pinned_root_sha256 = hashlib.sha256(pinned_root).hexdigest()
        has_current, previous_state = _load_trusted_state(
            state_root,
            working_metadata,
            pinned_root_sha256,
        )
        bootstrap = None if has_current else pinned_root
        updater = _PortableOfflineUpdater(
            str(working_metadata),
            "https://offline.reyn.invalid/metadata/",
            fetcher=_OfflineMetadataFetcher(metadata_source),
            config=UpdaterConfig(
                max_root_rotations=MAX_TUF_ROOT_ROTATIONS,
                max_delegations=MAX_TUF_DELEGATIONS,
                root_max_length=MAX_TUF_ROOT_BYTES,
                timestamp_max_length=MAX_TUF_TIMESTAMP_BYTES,
                snapshot_max_length=MAX_TUF_SNAPSHOT_BYTES,
                targets_max_length=MAX_TUF_TARGETS_BYTES,
                prefix_targets_with_hash=True,
            ),
            bootstrap=bootstrap,
        )
        model = payload["model"]
        target_path = (
            f"models/{model['id']}/{model['version']}/{path.name}"
        )
        signature_target_path = target_path + SIGNATURE_SUFFIX
        updater.refresh()
        target_info = updater.get_targetinfo(target_path)
        signature_info = updater.get_targetinfo(signature_target_path)
    except ModelBundleError:
        shutil.rmtree(working_root, ignore_errors=True)
        raise
    except Exception as exc:
        shutil.rmtree(working_root, ignore_errors=True)
        _raise_tuf_error(exc)

    try:
        if target_info is None or signature_info is None:
            _fail(
                "tuf.target_not_found",
                "tuf.targets",
                "delegated model bundle and signature targets are both required",
            )
        if target_info.length > MAX_BUNDLE_BYTES:
            _fail(
                "tuf.target_size",
                target_path,
                f"bundle target exceeds {MAX_BUNDLE_BYTES} bytes",
            )
        if signature_info.length > MAX_SIGNATURE_BYTES:
            _fail(
                "tuf.target_size",
                signature_target_path,
                f"signature target exceeds {MAX_SIGNATURE_BYTES} bytes",
            )
        with path.open("rb") as stream:
            target_info.verify_length_and_hashes(stream)
        with signature_path.open("rb") as stream:
            signature_info.verify_length_and_hashes(stream)
        if target_info.hashes.get("sha256") != bundle_sha256:
            _fail(
                "tuf.target_hash",
                target_path,
                "TUF target SHA-256 does not match the bundle",
            )

        root = _metadata_version(working_metadata / "root.json", Root)
        _validate_root_policy(root, "tuf.root")
        timestamp = _metadata_version(
            working_metadata / "timestamp.json", Timestamp
        )
        snapshot = _metadata_version(
            working_metadata / "snapshot.json", Snapshot
        )
        top_targets = _metadata_version(
            working_metadata / "targets.json", Targets
        )
        model_targets = _metadata_version(
            working_metadata / f"{TUF_MODELS_ROLE}.json", Targets
        )
        _require_meta_hashes(
            timestamp.signed.snapshot_meta, "tuf.timestamp.snapshot_meta"
        )
        for role_name in ("targets.json", f"{TUF_MODELS_ROLE}.json"):
            meta = snapshot.signed.meta.get(role_name)
            if meta is None:
                _fail(
                    "tuf.metadata_binding",
                    f"tuf.snapshot.meta.{role_name}",
                    "snapshot omitted required metadata",
                )
            _require_meta_hashes(meta, f"tuf.snapshot.meta.{role_name}")

        delegations = top_targets.signed.delegations
        delegated_role = (
            delegations.roles.get(TUF_MODELS_ROLE)
            if delegations is not None and delegations.roles is not None
            else None
        )
        if (
            delegated_role is None
            or not delegated_role.terminating
            or delegated_role.paths != ["models/*/*/*"]
            or delegated_role.threshold < TUF_MIN_TARGETS_THRESHOLD
        ):
            _fail(
                "tuf.delegation_confusion",
                "tuf.targets.delegations.models",
                "requires a terminating threshold models role scoped exactly to "
                "models/*/*/*",
            )
        if (
            target_path in top_targets.signed.targets
            or signature_target_path in top_targets.signed.targets
            or target_path not in model_targets.signed.targets
            or signature_target_path not in model_targets.signed.targets
        ):
            _fail(
                "tuf.delegation_confusion",
                "tuf.targets",
                "model artifacts must resolve only through the delegated models role",
            )
        custom, public_key = _target_custom(target_info, target_path)
        signature_custom = custom["detached_signature"]
        if signature_custom["target_path"] != signature_target_path:
            _fail(
                "tuf.target_custom",
                "tuf.target.custom.detached_signature.target_path",
                "detached signature target path does not match the model target",
            )
        if custom["model"] != payload["model"]:
            _fail(
                "tuf.target_custom",
                "tuf.target.custom.model",
                "TUF model identity does not match the detached signature",
            )
        if custom["release_sequence"] != payload["release_sequence"]:
            _fail(
                "tuf.target_custom",
                "tuf.target.custom.release_sequence",
                "TUF release sequence does not match the detached signature",
            )

        versions = {
            "root": root.signed.version,
            "timestamp": timestamp.signed.version,
            "snapshot": snapshot.signed.version,
            "targets": top_targets.signed.version,
            TUF_MODELS_ROLE: model_targets.signed.version,
        }
        for role_name, version in versions.items():
            previous = previous_state["metadata_versions"].get(role_name, 0)
            if (
                isinstance(previous, bool)
                or not isinstance(previous, int)
                or version < previous
            ):
                _fail(
                    "tuf.rollback",
                    f"tuf.metadata_versions.{role_name}",
                    f"metadata version {version} is below trusted version {previous}",
                )
        models = dict(previous_state["models"])
        if (
            len(models) > MAX_TUF_TRACKED_MODELS
            or (
                model["id"] not in models
                and len(models) >= MAX_TUF_TRACKED_MODELS
            )
        ):
            _fail(
                "tuf.trusted_state_invalid",
                "tuf.state.models",
                f"tracks more than {MAX_TUF_TRACKED_MODELS} models",
            )
        previous_model = models.get(model["id"])
        sequence = payload["release_sequence"]
        if previous_model is not None:
            previous_sequence = previous_model.get("release_sequence")
            previous_digest = previous_model.get("bundle_sha256")
            if sequence < previous_sequence:
                _fail(
                    "tuf.release_rollback",
                    "signature.signed.release_sequence",
                    f"release sequence {sequence} is below trusted sequence "
                    f"{previous_sequence}",
                )
            if sequence == previous_sequence and bundle_sha256 != previous_digest:
                _fail(
                    "tuf.release_sequence_reuse",
                    "signature.signed.release_sequence",
                    "release sequence was reused for different bundle bytes",
                )
        models[model["id"]] = {
            "release_sequence": sequence,
            "bundle_sha256": bundle_sha256,
            "model_version": model["version"],
        }
        state = {
            "schema": TUF_TRUSTED_STATE_SCHEMA,
            "pinned_root_sha256": pinned_root_sha256,
            "metadata_versions": versions,
            "models": models,
        }
        return {
            "working_root": working_root,
            "state": state,
            "target_path": target_path,
            "signature_target_path": signature_target_path,
            "signature_key_id": signature_custom["key_id"],
            "signature_public_key": public_key,
            "revoked_signature_key_ids": custom["revoked_signature_key_ids"],
            "metadata_versions": versions,
        }
    except ModelBundleError:
        shutil.rmtree(working_root, ignore_errors=True)
        raise
    except Exception as exc:
        shutil.rmtree(working_root, ignore_errors=True)
        _raise_tuf_error(exc)


def _verify_bundle_authenticity(
    path: Path,
    bundle_sha256: str,
    *,
    development_allow_unsigned: bool,
    trusted_state_dir=None,
) -> dict:
    signature_path = _signature_path(path)
    repository_path = _tuf_repository_path(path)
    if not signature_path.exists():
        if development_allow_unsigned and not repository_path.exists():
            return {
                "status": "development_unsigned_override",
                "verified": False,
                "algorithm": None,
                "key_id": None,
                "public_key_sha256": None,
                "release_sequence": None,
                "signature_path": None,
                "signed_bundle_schema": None,
                "signed_model": None,
                "tuf_target_path": None,
                "tuf_metadata_versions": None,
            }
        _fail(
            "signature.missing",
            "signature",
            f"required detached signature not found: {signature_path.name}",
        )
    if not signature_path.is_file() or signature_path.is_symlink():
        _fail(
            "signature.not_file",
            "signature",
            f"detached signature is not a regular non-symlink file: {signature_path.name}",
        )
    raw = _read_bounded(
        signature_path,
        MAX_SIGNATURE_BYTES,
        "signature.read_failed",
        "signature",
    )
    document, signature_bytes, issued_at, expires_at = _parse_signature_document(raw)
    payload = document["signed"]
    if payload["bundle_sha256"] != bundle_sha256:
        _fail(
            "signature.bundle_digest_mismatch",
            "signature.signed.bundle_sha256",
            "signed bundle SHA-256 does not match the bundle bytes",
        )

    with _locked_trusted_state(trusted_state_dir) as state_root:
        prepared = _prepare_tuf_authentication(
            path,
            signature_path,
            payload,
            bundle_sha256,
            state_root,
        )
        working_root = prepared["working_root"]
        try:
            if payload["key_id"] in prepared["revoked_signature_key_ids"]:
                _fail(
                    "signature.revoked_key",
                    "signature.signed.key_id",
                    f"publisher key {payload['key_id']!r} is revoked by TUF metadata",
                )
            if payload["key_id"] != prepared["signature_key_id"]:
                _fail(
                    "signature.untrusted_key",
                    "signature.signed.key_id",
                    "detached signature key is not authorized by TUF target metadata",
                )
            signed_bytes = SIGNATURE_DOMAIN + _canonical_json_bytes(payload)
            try:
                Ed25519PublicKey.from_public_bytes(
                    prepared["signature_public_key"]
                ).verify(signature_bytes, signed_bytes)
            except InvalidSignature:
                _fail(
                    "signature.invalid",
                    "signature.signature",
                    "Ed25519 signature verification failed",
                )
            except ValueError as exc:
                _fail(
                    "tuf.target_custom",
                    "tuf.target.custom.detached_signature.public_key",
                    f"invalid Ed25519 public key: {exc}",
                )
            now = datetime.now(timezone.utc)
            if issued_at > now + MAX_CLOCK_SKEW:
                _fail(
                    "signature.not_yet_valid",
                    "signature.signed.issued_at",
                    "signature issue time is in the future",
                )
            if expires_at <= now:
                _fail(
                    "signature.expired",
                    "signature.signed.expires_at",
                    "detached signature has expired",
                )
            _publish_trusted_state(
                state_root,
                working_root,
                prepared["state"],
            )
        finally:
            if working_root.exists():
                shutil.rmtree(working_root, ignore_errors=True)
    public_key = prepared["signature_public_key"]
    return {
        "status": "verified",
        "verified": True,
        "algorithm": SIGNATURE_ALGORITHM,
        "key_id": payload["key_id"],
        "public_key_sha256": hashlib.sha256(public_key).hexdigest(),
        "release_sequence": payload["release_sequence"],
        "issued_at": payload["issued_at"],
        "expires_at": payload["expires_at"],
        "signature_path": signature_path.name,
        "signed_bundle_schema": payload["bundle_schema"],
        "signed_model": dict(payload["model"]),
        "tuf_target_path": prepared["target_path"],
        "tuf_metadata_versions": dict(prepared["metadata_versions"]),
    }


def _reject_json_constant(value):
    _fail(
        "bundle.nonfinite_metadata",
        MANIFEST_NAME,
        f"JSON constant {value!r} is not permitted",
    )


def _reject_duplicate_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            _fail(
                "bundle.duplicate_manifest_key",
                MANIFEST_NAME,
                f"duplicate JSON key {key!r}",
            )
        result[key] = value
    return result


def _parse_canonical_manifest(raw: bytes) -> dict:
    try:
        manifest = json.loads(
            raw.decode("utf-8"),
            parse_constant=_reject_json_constant,
            object_pairs_hook=_reject_duplicate_keys,
        )
    except UnicodeDecodeError as exc:
        _fail("bundle.manifest_encoding", MANIFEST_NAME, f"must be UTF-8: {exc}")
    except json.JSONDecodeError as exc:
        _fail("bundle.manifest_json", MANIFEST_NAME, f"invalid JSON: {exc}")
    if not isinstance(manifest, dict):
        _fail("bundle.manifest_type", MANIFEST_NAME, "root must be an object")
    if raw != _canonical_json_bytes(manifest):
        _fail(
            "bundle.manifest_not_canonical",
            MANIFEST_NAME,
            "manifest must use Reyn canonical JSON encoding",
        )
    return manifest


def _sha256_file(path: Path, *, maximum=MAX_BUNDLE_BYTES) -> str:
    size = path.stat().st_size
    if size > maximum:
        _fail(
            "bundle.size_limit",
            str(path),
            f"{size} bytes exceeds the {maximum}-byte limit",
        )
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _validate_archive_path(name: str):
    if not name or "\\" in name:
        _fail("bundle.unsafe_path", "archive", f"unsafe member path {name!r}")
    path = PurePosixPath(name)
    if path.is_absolute() or any(part in ("", ".", "..") for part in path.parts):
        _fail("bundle.unsafe_path", "archive", f"unsafe member path {name!r}")
    if path.as_posix() != name:
        _fail("bundle.unsafe_path", "archive", f"non-canonical member path {name!r}")


def _validate_zip_infos(infos):
    for info in infos:
        _validate_archive_path(info.filename)
    if len(infos) != 2:
        _fail(
            "bundle.archive_inventory",
            "archive",
            f"expected exactly 2 members, found {len(infos)}",
        )
    names = [info.filename for info in infos]
    if len(set(names)) != len(names):
        _fail("bundle.duplicate_path", "archive", "member paths must be unique")
    for info in infos:
        if info.is_dir():
            _fail("bundle.archive_member", info.filename, "directories are not permitted")
        mode = info.external_attr >> 16
        if mode and stat.S_ISLNK(mode):
            _fail("bundle.archive_member", info.filename, "symbolic links are not permitted")
        if info.flag_bits & 0x1:
            _fail("bundle.archive_member", info.filename, "encrypted members are not permitted")
        if info.compress_type != zipfile.ZIP_STORED:
            _fail(
                "bundle.archive_compression",
                info.filename,
                "members must use ZIP_STORED for bounded deterministic loading",
            )
    expected = {MANIFEST_NAME, WEIGHTS_NAME}
    if set(names) != expected:
        _fail(
            "bundle.archive_inventory",
            "archive",
            f"members must be exactly {sorted(expected)}; found {sorted(names)}",
        )
    by_name = {info.filename: info for info in infos}
    manifest_info = by_name[MANIFEST_NAME]
    weights_info = by_name[WEIGHTS_NAME]
    if not 0 < manifest_info.file_size <= MAX_MANIFEST_BYTES:
        _fail(
            "bundle.size_limit",
            MANIFEST_NAME,
            f"manifest size must be 1..{MAX_MANIFEST_BYTES} bytes",
        )
    if not 0 < weights_info.file_size <= MAX_WEIGHTS_BYTES:
        _fail(
            "bundle.size_limit",
            WEIGHTS_NAME,
            f"weights size must be 1..{MAX_WEIGHTS_BYTES} bytes",
        )
    if manifest_info.file_size + weights_info.file_size > MAX_BUNDLE_BYTES:
        _fail(
            "bundle.size_limit",
            "archive",
            f"uncompressed payload exceeds {MAX_BUNDLE_BYTES} bytes",
        )
    return by_name


def _normalize_architecture(architecture_id: str, config: Mapping) -> dict:
    if not isinstance(config, Mapping):
        _fail("bundle.architecture_config", "architecture.config", "must be an object")
    raw = dict(config)
    if architecture_id == ARCHITECTURE_2D:
        allowed = {
            "in_channels",
            "out_channels",
            "width",
            "trunk_depth",
            "time_dim",
            "dt_scale",
            "param_dim",
            "param_embed_dim",
        }
        extra = sorted(set(raw) - allowed)
        if extra:
            _fail(
                "bundle.architecture_config",
                "architecture.config",
                f"unsupported DirectFlowMap option(s): {extra}",
            )
        in_channels = _bounded_int(
            raw.get("in_channels", 2),
            "architecture.config.in_channels",
            minimum=1,
            maximum=16,
        )
        out_channels = _bounded_int(
            raw.get("out_channels", in_channels),
            "architecture.config.out_channels",
            minimum=1,
            maximum=in_channels,
        )
        width = _bounded_int(
            raw.get("width", 96),
            "architecture.config.width",
            minimum=8,
            maximum=256,
        )
        if width % 8:
            _fail(
                "bundle.architecture_config",
                "architecture.config.width",
                "must be divisible by 8",
            )
        trunk_depth = _bounded_int(
            raw.get("trunk_depth", 15),
            "architecture.config.trunk_depth",
            minimum=1,
            maximum=64,
        )
        time_dim = _bounded_int(
            raw.get("time_dim", 64),
            "architecture.config.time_dim",
            minimum=2,
            maximum=512,
        )
        param_dim = _bounded_int(
            raw.get("param_dim", 0),
            "architecture.config.param_dim",
            minimum=0,
            maximum=32,
        )
        param_embed_dim = _bounded_int(
            raw.get("param_embed_dim", time_dim),
            "architecture.config.param_embed_dim",
            minimum=1,
            maximum=512,
        )
        dt_scale = _finite_number(
            raw.get("dt_scale", 0.01),
            "architecture.config.dt_scale",
            positive=True,
        )
        normalized = {
            "in_channels": in_channels,
            "out_channels": out_channels,
            "width": width,
            "trunk_depth": trunk_depth,
            "time_dim": time_dim,
            "dt_scale": dt_scale,
            "param_dim": param_dim,
            "param_embed_dim": param_embed_dim,
        }
    elif architecture_id == ARCHITECTURE_3D:
        allowed = {
            "in_channels",
            "out_channels",
            "width",
            "trunk_depth",
            "time_dim",
            "dt_scale",
            "dilations",
            "grad_checkpoint",
        }
        extra = sorted(set(raw) - allowed)
        if extra:
            _fail(
                "bundle.architecture_config",
                "architecture.config",
                f"unsupported DirectFlowMap3D option(s): {extra}",
            )
        in_channels = _bounded_int(
            raw.get("in_channels", 3),
            "architecture.config.in_channels",
            minimum=1,
            maximum=16,
        )
        out_channels = _bounded_int(
            raw.get("out_channels", in_channels),
            "architecture.config.out_channels",
            minimum=1,
            maximum=in_channels,
        )
        width = _bounded_int(
            raw.get("width", 64),
            "architecture.config.width",
            minimum=8,
            maximum=128,
        )
        if width % 8:
            _fail(
                "bundle.architecture_config",
                "architecture.config.width",
                "must be divisible by 8",
            )
        trunk_depth = _bounded_int(
            raw.get("trunk_depth", 8),
            "architecture.config.trunk_depth",
            minimum=1,
            maximum=32,
        )
        time_dim = _bounded_int(
            raw.get("time_dim", 64),
            "architecture.config.time_dim",
            minimum=2,
            maximum=512,
        )
        dt_scale = _finite_number(
            raw.get("dt_scale", 0.04),
            "architecture.config.dt_scale",
            positive=True,
        )
        dilations = raw.get("dilations", [1, 2, 4, 8])
        if not isinstance(dilations, (list, tuple)) or not 1 <= len(dilations) <= 8:
            _fail(
                "bundle.architecture_config",
                "architecture.config.dilations",
                "must contain 1..8 positive integers",
            )
        dilations = [
            _bounded_int(
                value,
                f"architecture.config.dilations[{index}]",
                minimum=1,
                maximum=32,
            )
            for index, value in enumerate(dilations)
        ]
        if len(set(dilations)) != len(dilations):
            _fail(
                "bundle.architecture_config",
                "architecture.config.dilations",
                "values must be unique",
            )
        grad_checkpoint = raw.get("grad_checkpoint", False)
        if grad_checkpoint is not False:
            _fail(
                "bundle.architecture_config",
                "architecture.config.grad_checkpoint",
                "inference bundles require grad_checkpoint=false",
            )
        normalized = {
            "in_channels": in_channels,
            "out_channels": out_channels,
            "width": width,
            "trunk_depth": trunk_depth,
            "time_dim": time_dim,
            "dt_scale": dt_scale,
            "dilations": dilations,
            "grad_checkpoint": False,
        }
    else:
        _fail(
            "bundle.unsupported_architecture",
            "architecture.id",
            f"unsupported architecture {architecture_id!r}",
        )
    return normalized


def _validate_support_envelope(value: dict, dimension: int) -> dict:
    _exact_keys(
        value,
        (
            "dimension",
            "grid_size",
            "scenario",
            "horizon_steps",
            "time_integration",
            "physics",
        ),
        "support_envelope",
    )
    if value["dimension"] != dimension:
        _fail(
            "bundle.support_mismatch",
            "support_envelope.dimension",
            f"architecture requires dimension {dimension}",
        )
    grid_size = _bounded_int(
        value["grid_size"],
        "support_envelope.grid_size",
        minimum=4,
        maximum=4096,
    )
    scenario = value["scenario"]
    if scenario not in ("free", "obstacle"):
        _fail(
            "bundle.support_mismatch",
            "support_envelope.scenario",
            "must be 'free' or 'obstacle'",
        )
    horizon = value["horizon_steps"]
    _exact_keys(horizon, ("min", "max"), "support_envelope.horizon_steps")
    minimum = _bounded_int(
        horizon["min"],
        "support_envelope.horizon_steps.min",
        minimum=1,
        maximum=1,
    )
    maximum = _bounded_int(
        horizon["max"],
        "support_envelope.horizon_steps.max",
        minimum=minimum,
        maximum=1_000_000,
    )
    integration = value["time_integration"]
    _exact_keys(
        integration,
        ("solver_dt", "stride", "frame_dt", "warmup_steps"),
        "support_envelope.time_integration",
    )
    solver_dt = _finite_number(
        integration["solver_dt"],
        "support_envelope.time_integration.solver_dt",
        positive=True,
    )
    stride = _bounded_int(
        integration["stride"],
        "support_envelope.time_integration.stride",
        minimum=1,
        maximum=1_000_000,
    )
    frame_dt = _finite_number(
        integration["frame_dt"],
        "support_envelope.time_integration.frame_dt",
        positive=True,
    )
    if frame_dt != solver_dt * stride:
        _fail(
            "bundle.support_mismatch",
            "support_envelope.time_integration.frame_dt",
            "must equal solver_dt * stride exactly",
        )
    warmup_steps = _bounded_int(
        integration["warmup_steps"],
        "support_envelope.time_integration.warmup_steps",
        minimum=0,
        maximum=10_000_000,
    )
    physics = value["physics"]
    if not isinstance(physics, dict):
        _fail("bundle.manifest_type", "support_envelope.physics", "must be an object")
    return {
        "dimension": dimension,
        "grid_size": grid_size,
        "scenario": scenario,
        "horizon_steps": {"min": minimum, "max": maximum},
        "time_integration": {
            "solver_dt": solver_dt,
            "stride": stride,
            "frame_dt": frame_dt,
            "warmup_steps": warmup_steps,
        },
        "physics": physics,
    }


def _semantics(architecture_id: str, config: dict, support: dict, physics_id: str):
    dimension = 2 if architecture_id == ARCHITECTURE_2D else 3
    in_channels = config["in_channels"]
    out_channels = config["out_channels"]
    param_dim = config.get("param_dim", 0)
    velocity = (
        ["velocity_x", "velocity_y"]
        if dimension == 2
        else ["velocity_x", "velocity_y", "velocity_z"]
    )
    spatial = []
    global_values = []
    normalization = {
        "dynamic_state": {"kind": "identity"},
        "time": {"kind": "divide_by", "scale": config["dt_scale"]},
        "spatial_conditions": {},
        "global_conditions": {},
    }
    physics = support["physics"]
    if dimension == 2 and (in_channels, out_channels, param_dim) == (2, 2, 0):
        expected_physics = "free_periodic.v1"
        packing = "velocity"
        if support["scenario"] != "free":
            _fail(
                "bundle.support_mismatch",
                "support_envelope.scenario",
                "2-channel 2D models require the free scenario",
            )
        if physics:
            _fail(
                "bundle.support_mismatch",
                "support_envelope.physics",
                "free 2D models must not declare fixed-body parameters",
            )
    elif dimension == 2 and (in_channels, out_channels, param_dim) == (3, 2, 0):
        expected_physics = "fixed_body_brinkman.v1"
        packing = "velocity+solid_fraction"
        spatial = ["solid_fraction"]
        normalization["spatial_conditions"] = {
            "solid_fraction": {"kind": "identity", "input_range": [0.0, 1.0]}
        }
        if support["scenario"] != "obstacle":
            _fail(
                "bundle.support_mismatch",
                "support_envelope.scenario",
                "mask-conditioned 2D models require the obstacle scenario",
            )
        if physics:
            _fail(
                "bundle.support_mismatch",
                "support_envelope.physics",
                "legacy mask conditioning has no supported parameter envelope",
            )
    elif dimension == 2 and (in_channels, out_channels, param_dim) == (4, 2, 1):
        expected_physics = "fixed_body_brinkman.v2"
        packing = "fixed_body_v2"
        spatial = ["solid_fraction", "sponge_coefficient"]
        global_values = ["log_kinematic_viscosity"]
        _exact_keys(
            physics,
            ("nu", "u_inf", "eta", "sponge_strength"),
            "support_envelope.physics",
        )
        nu = physics["nu"]
        if not isinstance(nu, list) or len(nu) != 2:
            _fail(
                "bundle.support_mismatch",
                "support_envelope.physics.nu",
                "must be [minimum, maximum]",
            )
        nu = [
            _finite_number(
                item,
                f"support_envelope.physics.nu[{index}]",
                positive=True,
            )
            for index, item in enumerate(nu)
        ]
        if nu[0] >= nu[1]:
            _fail(
                "bundle.support_mismatch",
                "support_envelope.physics.nu",
                "minimum must be less than maximum",
            )
        u_inf = physics["u_inf"]
        if not isinstance(u_inf, list) or len(u_inf) != 2:
            _fail(
                "bundle.support_mismatch",
                "support_envelope.physics.u_inf",
                "must contain two finite components",
            )
        u_inf = [
            _finite_number(
                item,
                f"support_envelope.physics.u_inf[{index}]",
            )
            for index, item in enumerate(u_inf)
        ]
        eta = _finite_number(
            physics["eta"], "support_envelope.physics.eta", positive=True
        )
        sponge_strength = _finite_number(
            physics["sponge_strength"],
            "support_envelope.physics.sponge_strength",
            positive=True,
        )
        canonical_physics = {
            "nu": nu,
            "u_inf": u_inf,
            "eta": eta,
            "sponge_strength": sponge_strength,
        }
        if physics != canonical_physics:
            _fail(
                "bundle.support_mismatch",
                "support_envelope.physics",
                "fixed-body support values must use canonical numeric forms",
            )
        normalization["spatial_conditions"] = {
            "solid_fraction": {"kind": "identity", "input_range": [0.0, 1.0]},
            "sponge_coefficient": {
                "kind": "divide_by",
                "scale": sponge_strength,
            },
        }
        normalization["global_conditions"] = {
            "log_kinematic_viscosity": {
                "kind": "log_affine",
                "input_bounds": nu,
                "output_bounds": [-1.0, 1.0],
            }
        }
        if support["scenario"] != "obstacle":
            _fail(
                "bundle.support_mismatch",
                "support_envelope.scenario",
                "fixed-body-v2 models require the obstacle scenario",
            )
    elif dimension == 3 and (in_channels, out_channels, param_dim) == (5, 3, 0):
        obstacle = True
        expected_physics = "fixed_body_brinkman.v1"
        packing = "velocity+solid_fraction+log_viscosity"
        spatial = ["solid_fraction"]
        global_values = ["log_kinematic_viscosity"]
        normalization["spatial_conditions"] = {
            "solid_fraction": {"kind": "identity", "input_range": [0.0, 1.0]}
        }
        _exact_keys(
            physics,
            ("kinematic_viscosity", "nu"),
            "support_envelope.physics",
        )
        _finite_number(
            physics["kinematic_viscosity"],
            "support_envelope.physics.kinematic_viscosity",
            positive=True,
        )
        nu = physics["nu"]
        if not isinstance(nu, (list, tuple)) or len(nu) != 2:
            _fail(
                "bundle.support_mismatch",
                "support_envelope.physics.nu",
                "5-channel 3D models require signed [min, max] viscosity bounds",
            )
        nu_lo = _finite_number(
            nu[0], "support_envelope.physics.nu[0]", positive=True
        )
        nu_hi = _finite_number(
            nu[1], "support_envelope.physics.nu[1]", positive=True
        )
        if nu_hi <= nu_lo:
            _fail(
                "bundle.support_mismatch",
                "support_envelope.physics.nu",
                "max viscosity must exceed min viscosity",
            )
        physics["nu"] = [nu_lo, nu_hi]
        normalization["global_conditions"] = {
            "log_kinematic_viscosity": {
                "kind": "log_affine",
                "input_bounds": physics["nu"],
                "output_bounds": [-1.0, 1.0],
            }
        }
        if support["scenario"] != "obstacle":
            _fail(
                "bundle.support_mismatch",
                "support_envelope.scenario",
                "5-channel 3D models require the obstacle scenario",
            )
    elif dimension == 3 and (in_channels, out_channels, param_dim) in (
        (3, 3, 0),
        (4, 3, 0),
    ):
        obstacle = in_channels == 4
        expected_physics = (
            "fixed_body_brinkman.v1" if obstacle else "free_periodic.v1"
        )
        packing = "velocity+solid_fraction" if obstacle else "velocity"
        if obstacle:
            spatial = ["solid_fraction"]
            normalization["spatial_conditions"] = {
                "solid_fraction": {"kind": "identity", "input_range": [0.0, 1.0]}
            }
        _exact_keys(physics, ("kinematic_viscosity",), "support_envelope.physics")
        _finite_number(
            physics["kinematic_viscosity"],
            "support_envelope.physics.kinematic_viscosity",
            positive=True,
        )
        expected_scenario = "obstacle" if obstacle else "free"
        if support["scenario"] != expected_scenario:
            _fail(
                "bundle.support_mismatch",
                "support_envelope.scenario",
                f"{in_channels}-channel 3D models require the {expected_scenario} scenario",
            )
    else:
        _fail(
            "bundle.unsupported_contract",
            "architecture.config",
            f"unsupported {dimension}D channel contract "
            f"in={in_channels}, out={out_channels}, params={param_dim}",
        )
    if physics_id != expected_physics:
        _fail(
            "bundle.conditioning_mismatch",
            "conditioning.physics_id",
            f"contract requires {expected_physics!r}, found {physics_id!r}",
        )
    io_schema = {
        "dynamic_inputs": velocity,
        "spatial_conditions": spatial,
        "global_conditions": global_values,
        "outputs": velocity,
    }
    conditioning = {
        "physics_id": expected_physics,
        "packing": packing,
        "parameter_dim": param_dim,
    }
    return io_schema, normalization, conditioning


def _validate_source_training(value):
    _exact_keys(
        value,
        (
            "source_fingerprint",
            "checkpoint_role",
            "epoch",
            "declared_epochs",
            "weight_variant",
            "training_seed",
            "dataset",
            "experiment_contract_sha256",
        ),
        "source_training",
    )
    fingerprint = value["source_fingerprint"]
    _exact_keys(
        fingerprint,
        ("algorithm", "digest"),
        "source_training.source_fingerprint",
    )
    if fingerprint["algorithm"] != "sha256" or not isinstance(
        fingerprint["digest"], str
    ) or not _SHA256.fullmatch(fingerprint["digest"]):
        _fail(
            "bundle.source_identity",
            "source_training.source_fingerprint",
            "requires algorithm='sha256' and a lowercase 64-hex digest",
        )
    role = value["checkpoint_role"]
    if not isinstance(role, str) or not role or len(role) > 128:
        _fail(
            "bundle.source_identity",
            "source_training.checkpoint_role",
            "must be a non-empty string up to 128 characters",
        )
    epoch = _bounded_int(
        value["epoch"], "source_training.epoch", minimum=0, maximum=10_000_000
    )
    declared = _bounded_int(
        value["declared_epochs"],
        "source_training.declared_epochs",
        minimum=1,
        maximum=10_000_000,
    )
    if epoch > declared:
        _fail(
            "bundle.source_identity",
            "source_training.epoch",
            "cannot exceed declared_epochs",
        )
    if value["weight_variant"] not in ("raw", "ema"):
        _fail(
            "bundle.source_identity",
            "source_training.weight_variant",
            "must be 'raw' or 'ema'",
        )
    _bounded_int(
        value["training_seed"],
        "source_training.training_seed",
        minimum=0,
        maximum=2**63 - 1,
    )
    dataset = value["dataset"]
    if not isinstance(dataset, str) or not dataset or len(dataset) > 256:
        _fail(
            "bundle.source_identity",
            "source_training.dataset",
            "must be a non-empty string up to 256 characters",
        )
    contract_digest = value["experiment_contract_sha256"]
    if contract_digest is not None and (
        not isinstance(contract_digest, str)
        or not _SHA256.fullmatch(contract_digest)
    ):
        _fail(
            "bundle.source_identity",
            "source_training.experiment_contract_sha256",
            "must be null or a lowercase 64-hex SHA-256 digest",
        )


def _validate_tensor_schema(entries):
    if not isinstance(entries, list) or not 1 <= len(entries) <= MAX_TENSORS:
        _fail(
            "bundle.tensor_count",
            "tensor_schema",
            f"must contain 1..{MAX_TENSORS} tensors",
        )
    names = []
    total_elements = 0
    for index, entry in enumerate(entries):
        field = f"tensor_schema[{index}]"
        _exact_keys(entry, ("name", "shape", "dtype"), field)
        name = entry["name"]
        if not isinstance(name, str) or not name or len(name) > 512:
            _fail("bundle.tensor_name", f"{field}.name", "must be a non-empty string")
        names.append(name)
        shape = entry["shape"]
        if not isinstance(shape, list) or len(shape) > MAX_TENSOR_RANK:
            _fail(
                "bundle.tensor_shape",
                f"{field}.shape",
                f"rank must not exceed {MAX_TENSOR_RANK}",
            )
        elements = 1
        for axis, size in enumerate(shape):
            size = _bounded_int(
                size,
                f"{field}.shape[{axis}]",
                minimum=0,
                maximum=MAX_TENSOR_ELEMENTS,
            )
            elements *= size
            if elements > MAX_TENSOR_ELEMENTS:
                _fail(
                    "bundle.tensor_shape",
                    f"{field}.shape",
                    f"tensor exceeds {MAX_TENSOR_ELEMENTS} elements",
                )
        total_elements += elements
        if total_elements > MAX_TENSOR_ELEMENTS:
            _fail(
                "bundle.tensor_shape",
                "tensor_schema",
                f"bundle exceeds {MAX_TENSOR_ELEMENTS} tensor elements",
            )
        if entry["dtype"] not in _TORCH_DTYPE_NAMES.values():
            _fail(
                "bundle.tensor_dtype",
                f"{field}.dtype",
                f"unsupported dtype {entry['dtype']!r}",
            )
    if names != sorted(names) or len(set(names)) != len(names):
        _fail(
            "bundle.tensor_keys",
            "tensor_schema",
            "tensor names must be unique and sorted",
        )


def validate_manifest(manifest: dict) -> dict:
    _exact_keys(manifest, _TOP_LEVEL_KEYS, MANIFEST_NAME)
    if manifest["schema"] != BUNDLE_SCHEMA:
        _fail(
            "bundle.unsupported_schema",
            "schema",
            f"expected {BUNDLE_SCHEMA!r}, found {manifest['schema']!r}",
        )
    model = manifest["model"]
    _exact_keys(model, ("id", "version"), "model")
    for field in ("id", "version"):
        if not isinstance(model[field], str) or not _SAFE_NAME.fullmatch(model[field]):
            _fail(
                "bundle.model_identity",
                f"model.{field}",
                "must match [A-Za-z0-9][A-Za-z0-9._-]{0,127}",
            )
    architecture = manifest["architecture"]
    _exact_keys(architecture, _ARCHITECTURE_KEYS, "architecture")
    architecture_id = architecture["id"]
    config = _normalize_architecture(architecture_id, architecture["config"])
    if architecture["config"] != config:
        _fail(
            "bundle.architecture_config",
            "architecture.config",
            "configuration must be fully materialized in canonical form",
        )
    dimension = 2 if architecture_id == ARCHITECTURE_2D else 3
    support = _validate_support_envelope(manifest["support_envelope"], dimension)
    conditioning = manifest["conditioning"]
    if not isinstance(conditioning, dict):
        _fail("bundle.manifest_type", "conditioning", "must be an object")
    physics_id = conditioning.get("physics_id")
    io_schema, normalization, expected_conditioning = _semantics(
        architecture_id, config, support, physics_id
    )
    if manifest["io_schema"] != io_schema:
        _fail(
            "bundle.schema_mismatch",
            "io_schema",
            "does not match the declared architecture and conditioning contract",
        )
    if manifest["normalization"] != normalization:
        _fail(
            "bundle.normalization_mismatch",
            "normalization",
            "does not match the declared conditioning support",
        )
    if conditioning != expected_conditioning:
        _fail(
            "bundle.conditioning_mismatch",
            "conditioning",
            "does not match the declared architecture and support envelope",
        )
    _validate_source_training(manifest["source_training"])
    limitations = manifest["limitations"]
    if not isinstance(limitations, list) or any(
        not isinstance(value, str) or not value or len(value) > 1024
        for value in limitations
    ):
        _fail(
            "bundle.limitations",
            "limitations",
            "must be a list of non-empty strings up to 1024 characters",
        )
    reports = manifest["benchmark_reports"]
    if not isinstance(reports, list) or any(
        not isinstance(value, str) or not _SHA256.fullmatch(value)
        for value in reports
    ):
        _fail(
            "bundle.report_hash",
            "benchmark_reports",
            "must contain only lowercase SHA-256 digests",
        )
    if reports != sorted(set(reports)):
        _fail(
            "bundle.report_hash",
            "benchmark_reports",
            "hashes must be unique and sorted",
        )
    _validate_tensor_schema(manifest["tensor_schema"])
    files = manifest["files"]
    if not isinstance(files, list) or len(files) != 1:
        _fail(
            "bundle.file_inventory",
            "files",
            "must inventory exactly weights.safetensors",
        )
    entry = files[0]
    _exact_keys(entry, ("path", "size_bytes", "sha256"), "files[0]")
    _validate_archive_path(entry["path"] if isinstance(entry["path"], str) else "")
    if entry["path"] != WEIGHTS_NAME:
        _fail(
            "bundle.file_inventory",
            "files[0].path",
            f"must be {WEIGHTS_NAME!r}",
        )
    _bounded_int(
        entry["size_bytes"],
        "files[0].size_bytes",
        minimum=1,
        maximum=MAX_WEIGHTS_BYTES,
    )
    if not isinstance(entry["sha256"], str) or not _SHA256.fullmatch(entry["sha256"]):
        _fail(
            "bundle.file_inventory",
            "files[0].sha256",
            "must be a lowercase SHA-256 digest",
        )
    return manifest


def _model_from_manifest(manifest: dict) -> torch.nn.Module:
    architecture = manifest["architecture"]
    config = dict(architecture["config"])
    try:
        if architecture["id"] == ARCHITECTURE_2D:
            from time_moe_operator import DirectFlowMap

            return DirectFlowMap(**config)
        if architecture["id"] == ARCHITECTURE_3D:
            from models_3d import DirectFlowMap3D

            config["dilations"] = tuple(config["dilations"])
            return DirectFlowMap3D(**config)
    except Exception as exc:
        _fail(
            "bundle.model_construction",
            "architecture.config",
            f"could not construct bounded architecture: {exc}",
        )
    _fail(
        "bundle.unsupported_architecture",
        "architecture.id",
        f"unsupported architecture {architecture['id']!r}",
    )


def _tensor_schema(state: Mapping[str, torch.Tensor]) -> list[dict]:
    entries = []
    for name in sorted(state):
        tensor = state[name]
        if not isinstance(name, str) or not name:
            _fail("bundle.tensor_name", "model_state_dict", "tensor names must be strings")
        if not torch.is_tensor(tensor) or tensor.layout != torch.strided:
            _fail(
                "bundle.tensor_type",
                f"model_state_dict.{name}",
                "all state values must be dense tensors",
            )
        dtype = _TORCH_DTYPE_NAMES.get(tensor.dtype)
        if dtype is None:
            _fail(
                "bundle.tensor_dtype",
                f"model_state_dict.{name}",
                f"unsupported dtype {tensor.dtype}",
            )
        if tensor.ndim > MAX_TENSOR_RANK or tensor.numel() > MAX_TENSOR_ELEMENTS:
            _fail(
                "bundle.tensor_shape",
                f"model_state_dict.{name}",
                "tensor exceeds the bundle shape limit",
            )
        entries.append(
            {"name": name, "shape": list(tensor.shape), "dtype": dtype}
        )
    _validate_tensor_schema(entries)
    return entries


def _verify_expected_state(
    expected: Mapping[str, torch.Tensor],
    actual_schema: list[dict],
    tensors: Mapping[str, torch.Tensor] | None = None,
):
    expected_schema = _tensor_schema(expected)
    expected_by_name = {entry["name"]: entry for entry in expected_schema}
    actual_by_name = {entry["name"]: entry for entry in actual_schema}
    missing = sorted(set(expected_by_name) - set(actual_by_name))
    extra = sorted(set(actual_by_name) - set(expected_by_name))
    if missing or extra:
        details = []
        if missing:
            details.append(f"missing {missing}")
        if extra:
            details.append(f"unexpected {extra}")
        _fail("bundle.tensor_keys", "tensor_schema", "; ".join(details))
    for name in sorted(expected_by_name):
        expected_entry = expected_by_name[name]
        actual_entry = actual_by_name[name]
        if actual_entry["shape"] != expected_entry["shape"]:
            _fail(
                "bundle.tensor_shape",
                f"tensor_schema.{name}",
                f"expected {expected_entry['shape']}, found {actual_entry['shape']}",
            )
        if actual_entry["dtype"] != expected_entry["dtype"]:
            _fail(
                "bundle.tensor_dtype",
                f"tensor_schema.{name}",
                f"expected {expected_entry['dtype']}, found {actual_entry['dtype']}",
            )
        if tensors is not None:
            tensor = tensors[name]
            if tensor.is_floating_point() and not torch.isfinite(tensor).all().item():
                _fail(
                    "bundle.tensor_nonfinite",
                    f"weights.{name}",
                    "contains NaN or Inf",
                )


@contextmanager
def _verified_payload(
    path,
    *,
    development_allow_unsigned=False,
    skip_authentication=False,
    trusted_state_dir=None,
):
    path = Path(path).expanduser().resolve()
    if not path.is_file():
        _fail("bundle.not_found", "path", f"bundle does not exist: {path}")
    if path.suffix.lower() != BUNDLE_EXTENSION:
        _fail(
            "bundle.invalid_extension",
            "path",
            f"production models require the {BUNDLE_EXTENSION} extension",
        )
    size = path.stat().st_size
    if not 0 < size <= MAX_BUNDLE_BYTES:
        _fail(
            "bundle.size_limit",
            "path",
            f"bundle size must be 1..{MAX_BUNDLE_BYTES} bytes",
        )
    bundle_sha256 = _sha256_file(path)
    if skip_authentication:
        authenticity = {
            "status": "offline_structural_validation",
            "verified": False,
            "algorithm": None,
            "key_id": None,
            "public_key_sha256": None,
            "release_sequence": None,
            "signature_path": None,
            "signed_bundle_schema": None,
            "signed_model": None,
            "tuf_target_path": None,
            "tuf_metadata_versions": None,
        }
    else:
        authenticity = _verify_bundle_authenticity(
            path,
            bundle_sha256,
            development_allow_unsigned=development_allow_unsigned,
            trusted_state_dir=trusted_state_dir,
        )
    temporary_path = None
    try:
        try:
            archive = zipfile.ZipFile(path, "r")
        except zipfile.BadZipFile as exc:
            _fail("bundle.invalid_archive", "path", f"invalid ZIP archive: {exc}")
        with archive:
            infos = _validate_zip_infos(archive.infolist())
            raw_manifest = archive.read(MANIFEST_NAME)
            if len(raw_manifest) != infos[MANIFEST_NAME].file_size:
                _fail(
                    "bundle.truncated_member",
                    MANIFEST_NAME,
                    "member size does not match the ZIP directory",
                )
            manifest = _parse_canonical_manifest(raw_manifest)
            validate_manifest(manifest)
            if authenticity["verified"] and (
                authenticity["signed_bundle_schema"] != manifest["schema"]
                or authenticity["signed_model"] != manifest["model"]
            ):
                _fail(
                    "signature.payload_manifest_mismatch",
                    "signature.signed",
                    "signed schema/model identity does not match the authenticated manifest",
                )
            inventory = manifest["files"][0]
            if inventory["size_bytes"] != infos[WEIGHTS_NAME].file_size:
                _fail(
                    "bundle.file_size_mismatch",
                    WEIGHTS_NAME,
                    f"manifest declares {inventory['size_bytes']} bytes, "
                    f"archive contains {infos[WEIGHTS_NAME].file_size}",
                )
            with tempfile.NamedTemporaryFile(
                prefix="reyn-weights-", suffix=".safetensors", delete=False
            ) as temporary:
                temporary_path = Path(temporary.name)
                digest = hashlib.sha256()
                copied = 0
                with archive.open(WEIGHTS_NAME, "r") as source:
                    while True:
                        chunk = source.read(1024 * 1024)
                        if not chunk:
                            break
                        copied += len(chunk)
                        if copied > MAX_WEIGHTS_BYTES:
                            _fail(
                                "bundle.size_limit",
                                WEIGHTS_NAME,
                                "weights exceed the extraction limit",
                            )
                        digest.update(chunk)
                        temporary.write(chunk)
            if copied != inventory["size_bytes"]:
                _fail(
                    "bundle.truncated_member",
                    WEIGHTS_NAME,
                    f"expected {inventory['size_bytes']} bytes, read {copied}",
                )
            if digest.hexdigest() != inventory["sha256"]:
                _fail(
                    "bundle.sha256_mismatch",
                    WEIGHTS_NAME,
                    "payload SHA-256 does not match the manifest",
                )
        yield manifest, temporary_path, authenticity, bundle_sha256
    except ModelBundleError:
        raise
    except (OSError, RuntimeError, zipfile.BadZipFile) as exc:
        _fail(
            "bundle.archive_read",
            "archive",
            f"could not read the bounded bundle payload: {exc}",
        )
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)


def _load_model_bundle(
    path,
    *,
    development_allow_unsigned=False,
    skip_authentication=False,
    trusted_state_dir=None,
) -> LoadedModelBundle:
    with _verified_payload(
        path,
        development_allow_unsigned=development_allow_unsigned,
        skip_authentication=skip_authentication,
        trusted_state_dir=trusted_state_dir,
    ) as (manifest, weights_path, authenticity, _bundle_sha256):
        try:
            with weights_path.open("rb") as weights_stream:
                prefix = weights_stream.read(8)
            if len(prefix) != 8:
                _fail(
                    "bundle.invalid_safetensors",
                    WEIGHTS_NAME,
                    "payload is shorter than the Safetensors header prefix",
                )
            header_size = int.from_bytes(prefix, "little", signed=False)
            if not 2 <= header_size <= MAX_SAFETENSORS_HEADER_BYTES:
                _fail(
                    "bundle.safetensors_header_limit",
                    WEIGHTS_NAME,
                    f"header size must be 2..{MAX_SAFETENSORS_HEADER_BYTES} bytes",
                )
            if header_size + 8 > weights_path.stat().st_size:
                _fail(
                    "bundle.invalid_safetensors",
                    WEIGHTS_NAME,
                    "declared Safetensors header exceeds the payload size",
                )
            with safe_open(
                weights_path, framework="pt", device="cpu"
            ) as tensor_file:
                keys = sorted(tensor_file.keys())
                if not 1 <= len(keys) <= MAX_TENSORS:
                    _fail(
                        "bundle.tensor_count",
                        WEIGHTS_NAME,
                        f"must contain 1..{MAX_TENSORS} tensors",
                    )
                header_schema = []
                for name in keys:
                    tensor_slice = tensor_file.get_slice(name)
                    safe_dtype = tensor_slice.get_dtype()
                    dtype = _SAFE_DTYPE_NAMES.get(safe_dtype)
                    if dtype is None:
                        _fail(
                            "bundle.tensor_dtype",
                            f"weights.{name}",
                            f"unsupported Safetensors dtype {safe_dtype!r}",
                        )
                    header_schema.append(
                        {
                            "name": name,
                            "shape": list(tensor_slice.get_shape()),
                            "dtype": dtype,
                        }
                    )
                _validate_tensor_schema(header_schema)
                if header_schema != manifest["tensor_schema"]:
                    _fail(
                        "bundle.tensor_manifest_mismatch",
                        "tensor_schema",
                        "Safetensors header does not match the manifest tensor inventory",
                    )
                model = _model_from_manifest(manifest)
                _verify_expected_state(model.state_dict(), header_schema)
                tensors = {name: tensor_file.get_tensor(name) for name in keys}
        except ModelBundleError:
            raise
        except Exception as exc:
            _fail(
                "bundle.invalid_safetensors",
                WEIGHTS_NAME,
                f"could not read Safetensors payload: {exc}",
            )
        _verify_expected_state(model.state_dict(), header_schema, tensors)
        try:
            incompatible = model.load_state_dict(tensors, strict=True)
        except RuntimeError as exc:
            _fail(
                "bundle.state_load",
                WEIGHTS_NAME,
                f"strict model state load failed: {exc}",
            )
        if incompatible.missing_keys or incompatible.unexpected_keys:
            _fail(
                "bundle.tensor_keys",
                WEIGHTS_NAME,
                f"missing={incompatible.missing_keys}, unexpected={incompatible.unexpected_keys}",
            )
        model.eval()
        return LoadedModelBundle(
            manifest=manifest,
            model=model,
            authenticity=authenticity,
        )


def load_model_bundle(
    path,
    *,
    development_allow_unsigned=False,
    trusted_state_dir=None,
) -> LoadedModelBundle:
    """Authenticate, validate, instantiate, and load a model bundle.

    ``trusted_state_dir`` is required for production and persists the highest
    authenticated metadata versions and per-model release sequence. The
    ``development_allow_unsigned`` escape hatch applies only when both the
    detached signature and TUF repository are absent; it never bypasses malformed
    signatures or untrusted metadata. Reyn Studio never enables it, and no
    environment variable or persisted setting can enable it.
    """

    if not isinstance(development_allow_unsigned, bool):
        _fail(
            "signature.development_override",
            "development_allow_unsigned",
            "must be an explicit boolean",
        )
    return _load_model_bundle(
        path,
        development_allow_unsigned=development_allow_unsigned,
        trusted_state_dir=trusted_state_dir,
    )


def verify_model_bundle(
    path,
    *,
    development_allow_unsigned=False,
    trusted_state_dir=None,
) -> dict:
    """Return a compact verification summary; raises ``ModelBundleError`` on failure."""

    loaded = load_model_bundle(
        path,
        development_allow_unsigned=development_allow_unsigned,
        trusted_state_dir=trusted_state_dir,
    )
    manifest = loaded.manifest
    bundle_path = Path(path).expanduser().resolve()
    return {
        "ok": True,
        "schema": manifest["schema"],
        "model": dict(manifest["model"]),
        "architecture": manifest["architecture"]["id"],
        "tensor_count": len(manifest["tensor_schema"]),
        "bundle_size_bytes": bundle_path.stat().st_size,
        "bundle_sha256": _sha256_file(bundle_path),
        "weights_sha256": manifest["files"][0]["sha256"],
        "integrity": {
            "algorithm": "sha256",
            "bundle_sha256": _sha256_file(bundle_path),
            "weights_sha256": manifest["files"][0]["sha256"],
        },
        "authenticity": dict(loaded.authenticity),
    }


def _load_private_signing_key(path, *, password=None) -> Ed25519PrivateKey:
    key_path = Path(os.path.abspath(Path(path).expanduser()))
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    descriptor = None
    try:
        descriptor = os.open(key_path, flags)
        key_stat = os.fstat(descriptor)
        if not stat.S_ISREG(key_stat.st_mode):
            _fail(
                "signing.private_key_type",
                "private_key",
                "private key must be a regular file",
            )
        if key_stat.st_mode & 0o077:
            _fail(
                "signing.private_key_permissions",
                "private_key",
                "private key permissions must deny all group/other access (mode 0600 or stricter)",
            )
        if not 0 < key_stat.st_size <= MAX_SIGNATURE_BYTES:
            _fail(
                "signing.private_key_size",
                "private_key",
                f"private key size must be 1..{MAX_SIGNATURE_BYTES} bytes",
            )
        with os.fdopen(descriptor, "rb") as stream:
            descriptor = None
            key_bytes = stream.read(MAX_SIGNATURE_BYTES + 1)
    except ModelBundleError:
        raise
    except OSError as exc:
        _fail(
            "signing.private_key_read",
            "private_key",
            f"could not safely open protected private key: {exc}",
        )
    finally:
        if descriptor is not None:
            os.close(descriptor)
    try:
        key = serialization.load_pem_private_key(key_bytes, password=password)
    except (TypeError, ValueError) as exc:
        _fail(
            "signing.private_key_format",
            "private_key",
            "requires a PKCS#8 Ed25519 PEM and the correct optional passphrase: "
            f"{exc}",
        )
    if not isinstance(key, Ed25519PrivateKey):
        _fail(
            "signing.private_key_algorithm",
            "private_key",
            "private key must use Ed25519",
        )
    return key


def sign_model_bundle(
    path,
    *,
    private_key_path,
    key_id,
    release_sequence,
    issued_at,
    expires_at,
    signature_path=None,
    private_key_password=None,
) -> dict:
    """Create a deterministic detached signature in an explicit offline operation."""

    bundle_path = Path(path).expanduser().resolve()
    loaded = _load_model_bundle(bundle_path, skip_authentication=True)
    payload = {
        "schema": SIGNATURE_PAYLOAD_SCHEMA,
        "algorithm": SIGNATURE_ALGORITHM,
        "key_id": key_id,
        "bundle_schema": loaded.manifest["schema"],
        "bundle_sha256": _sha256_file(bundle_path),
        "model": dict(loaded.manifest["model"]),
        "release_sequence": release_sequence,
        "issued_at": issued_at,
        "expires_at": expires_at,
    }
    if private_key_password is not None and not isinstance(private_key_password, bytes):
        _fail(
            "signing.private_key_password",
            "private_key_password",
            "must be bytes when supplied",
        )
    private_key = _load_private_signing_key(
        private_key_path,
        password=private_key_password,
    )
    signature = private_key.sign(SIGNATURE_DOMAIN + _canonical_json_bytes(payload))
    document = {
        "schema": SIGNATURE_DOCUMENT_SCHEMA,
        "signed": payload,
        "signature": base64.b64encode(signature).decode("ascii"),
    }
    raw = _canonical_json_bytes(document)
    _parse_signature_document(raw)
    destination = (
        Path(signature_path).expanduser().resolve()
        if signature_path is not None
        else _signature_path(bundle_path)
    )
    if destination == bundle_path:
        _fail(
            "signing.output_path",
            "signature",
            "detached signature path must differ from the bundle path",
        )
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary_path = None
    try:
        with tempfile.NamedTemporaryFile(
            prefix=f".{destination.name}.",
            suffix=".tmp",
            dir=destination.parent,
            delete=False,
        ) as temporary:
            temporary_path = Path(temporary.name)
            temporary.write(raw)
            temporary.flush()
            os.fsync(temporary.fileno())
        os.chmod(temporary_path, 0o644)
        os.replace(temporary_path, destination)
        temporary_path = None
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)
    public_key = private_key.public_key().public_bytes(
        serialization.Encoding.Raw,
        serialization.PublicFormat.Raw,
    )
    return {
        "ok": True,
        "signature_path": str(destination),
        "schema": SIGNATURE_DOCUMENT_SCHEMA,
        "algorithm": SIGNATURE_ALGORITHM,
        "key_id": key_id,
        "public_key_sha256": hashlib.sha256(public_key).hexdigest(),
        "release_sequence": release_sequence,
        "bundle_sha256": payload["bundle_sha256"],
        "model": dict(payload["model"]),
        "issued_at": issued_at,
        "expires_at": expires_at,
    }


def _support_from_checkpoint(checkpoint: Mapping, architecture_id: str) -> dict:
    train_args = checkpoint.get("train_args")
    if not isinstance(train_args, Mapping):
        _fail(
            "bundle.missing_metadata",
            "train_args",
            "trusted checkpoint must contain a train_args mapping",
        )
    grid_size = _bounded_int(
        train_args.get("grid_size"),
        "train_args.grid_size",
        minimum=4,
        maximum=4096,
    )
    max_steps = _bounded_int(
        train_args.get("max_steps"),
        "train_args.max_steps",
        minimum=1,
        maximum=1_000_000,
    )
    solver_dt = _finite_number(
        train_args.get("dt"), "train_args.dt", positive=True
    )
    stride = _bounded_int(
        train_args.get("stride"),
        "train_args.stride",
        minimum=1,
        maximum=1_000_000,
    )
    warmup_steps = _bounded_int(
        train_args.get("warmup_steps"),
        "train_args.warmup_steps",
        minimum=0,
        maximum=10_000_000,
    )
    config = checkpoint["model_config"]
    scenario = train_args.get("scenario")
    if scenario is None:
        scenario = (
            "obstacle"
            if int(config["in_channels"]) > int(config.get("out_channels") or config["in_channels"])
            else "free"
        )
    if scenario not in ("free", "obstacle"):
        _fail(
            "bundle.missing_metadata",
            "train_args.scenario",
            f"unsupported scenario {scenario!r}",
        )
    physics = {}
    if architecture_id == ARCHITECTURE_2D and (
        int(config["in_channels"]),
        int(config.get("out_channels") or config["in_channels"]),
        int(config.get("param_dim") or 0),
    ) == (4, 2, 1):
        physics_spec = checkpoint.get("physics_spec")
        support = (
            physics_spec.get("support")
            if isinstance(physics_spec, Mapping)
            else None
        )
        if not isinstance(support, Mapping):
            _fail(
                "bundle.missing_metadata",
                "physics_spec.support",
                "fixed-body-v2 conversion requires support metadata",
            )
        physics = {
            "nu": list(support.get("nu") or []),
            "u_inf": list(support.get("u_inf") or []),
            "eta": support.get("eta"),
            "sponge_strength": support.get("sponge_strength"),
        }
    elif architecture_id == ARCHITECTURE_3D:
        physics = {"kinematic_viscosity": train_args.get("nu")}
        if int(config["in_channels"]) == 5:
            bounds = train_args.get("nu_bounds")
            if not isinstance(bounds, (list, tuple)) or len(bounds) != 2:
                _fail(
                    "bundle.missing_metadata",
                    "train_args.nu_bounds",
                    "5-channel 3D conversion requires nu_bounds [min, max]",
                )
            physics["nu"] = [float(bounds[0]), float(bounds[1])]
    return {
        "dimension": 2 if architecture_id == ARCHITECTURE_2D else 3,
        "grid_size": grid_size,
        "scenario": scenario,
        "horizon_steps": {"min": 1, "max": max_steps},
        "time_integration": {
            "solver_dt": solver_dt,
            "stride": stride,
            "frame_dt": solver_dt * stride,
            "warmup_steps": warmup_steps,
        },
        "physics": physics,
    }


def _source_training_from_checkpoint(
    checkpoint: Mapping, *, source_digest: str | None = None
) -> dict:
    train_args = checkpoint.get("train_args")
    train_args = train_args if isinstance(train_args, Mapping) else {}
    source = checkpoint.get("source_fingerprint")
    source = source if isinstance(source, Mapping) else {}
    digest = source_digest or source.get("digest")
    algorithm = "sha256" if source_digest else source.get("algorithm", "sha256")
    role = checkpoint.get("checkpoint_role")
    epoch = checkpoint.get("epoch")
    declared_epochs = train_args.get("epochs")
    seed = train_args.get("seed")
    dataset = train_args.get("dataset")
    variant = checkpoint.get("weight_variant")
    if variant is None:
        variant = "ema" if isinstance(role, str) and role.endswith("_ema") else "raw"
    experiment_contract = checkpoint.get("experiment_contract")
    contract_digest = (
        hashlib.sha256(_canonical_json_bytes(experiment_contract)).hexdigest()
        if isinstance(experiment_contract, dict)
        else None
    )
    value = {
        "source_fingerprint": {"algorithm": algorithm, "digest": digest},
        "checkpoint_role": role,
        "epoch": epoch,
        "declared_epochs": declared_epochs,
        "weight_variant": variant,
        "training_seed": seed,
        "dataset": dataset,
        "experiment_contract_sha256": contract_digest,
    }
    _validate_source_training(value)
    return value


def _report_hashes(checkpoint: Mapping) -> list[str]:
    values = checkpoint.get("benchmark_reports")
    if values is None:
        return []
    if not isinstance(values, (list, tuple)):
        _fail(
            "bundle.report_hash",
            "benchmark_reports",
            "checkpoint field must be a list",
        )
    hashes = []
    for value in values:
        digest = value.get("sha256") if isinstance(value, Mapping) else value
        if not isinstance(digest, str) or not _SHA256.fullmatch(digest):
            _fail(
                "bundle.report_hash",
                "benchmark_reports",
                f"invalid benchmark report hash {digest!r}",
            )
        hashes.append(digest)
    return sorted(set(hashes))


def _zip_info(name: str) -> zipfile.ZipInfo:
    info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
    info.compress_type = zipfile.ZIP_STORED
    info.create_system = 3
    info.external_attr = (stat.S_IFREG | 0o644) << 16
    info.flag_bits = 0x800
    return info


def write_model_bundle(
    checkpoint: Mapping,
    destination,
    *,
    model_id: str,
    model_version: str,
    source_digest: str | None = None,
) -> dict:
    """Convert an already trusted in-memory checkpoint mapping to a safe bundle.

    This function intentionally accepts an in-memory mapping and never calls
    ``torch.load``.  The explicit trusted offline CLI is the only pickle bridge.
    """

    if not isinstance(checkpoint, Mapping):
        _fail("bundle.checkpoint_root", "checkpoint", "must be a mapping")
    state = checkpoint.get("model_state_dict")
    config = checkpoint.get("model_config")
    if not isinstance(state, Mapping) or not isinstance(config, Mapping):
        _fail(
            "bundle.missing_metadata",
            "checkpoint",
            "requires model_state_dict and model_config mappings",
        )
    state = dict(state)
    is3d = any(
        torch.is_tensor(tensor) and tensor.ndim == 5 for tensor in state.values()
    )
    architecture_id = ARCHITECTURE_3D if is3d else ARCHITECTURE_2D
    normalized_config = _normalize_architecture(architecture_id, config)
    architecture = {"id": architecture_id, "config": normalized_config}
    support = _support_from_checkpoint(checkpoint, architecture_id)
    physics_spec = checkpoint.get("physics_spec")
    if architecture_id == ARCHITECTURE_2D:
        in_channels = normalized_config["in_channels"]
        out_channels = normalized_config["out_channels"]
        param_dim = normalized_config["param_dim"]
        if (in_channels, out_channels, param_dim) == (4, 2, 1):
            physics_id = (
                physics_spec.get("physics_id")
                if isinstance(physics_spec, Mapping)
                else None
            )
        elif (in_channels, out_channels, param_dim) == (3, 2, 0):
            physics_id = "fixed_body_brinkman.v1"
        else:
            physics_id = "free_periodic.v1"
    else:
        physics_id = (
            "fixed_body_brinkman.v1"
            if normalized_config["in_channels"] in (4, 5)
            else "free_periodic.v1"
        )
    io_schema, normalization, conditioning = _semantics(
        architecture_id, normalized_config, support, physics_id
    )
    model_stub_manifest = {"architecture": architecture}
    model = _model_from_manifest(model_stub_manifest)
    schema = _tensor_schema(state)
    _verify_expected_state(model.state_dict(), schema, state)
    safe_state = {
        name: state[name].detach().cpu().contiguous() for name in sorted(state)
    }
    try:
        weights = save_safetensors(safe_state)
    except Exception as exc:
        _fail(
            "bundle.safetensors_write",
            "model_state_dict",
            f"could not serialize tensor-only weights: {exc}",
        )
    if not 0 < len(weights) <= MAX_WEIGHTS_BYTES:
        _fail(
            "bundle.size_limit",
            WEIGHTS_NAME,
            f"serialized weights exceed {MAX_WEIGHTS_BYTES} bytes",
        )
    limitations_value = checkpoint.get("limitations") or []
    if not isinstance(limitations_value, (list, tuple)):
        _fail(
            "bundle.limitations",
            "limitations",
            "checkpoint limitations must be a list",
        )
    limitations = [str(value) for value in limitations_value]
    manifest = {
        "schema": BUNDLE_SCHEMA,
        "model": {"id": model_id, "version": model_version},
        "architecture": architecture,
        "io_schema": io_schema,
        "normalization": normalization,
        "conditioning": conditioning,
        "support_envelope": support,
        "source_training": _source_training_from_checkpoint(
            checkpoint, source_digest=source_digest
        ),
        "limitations": limitations,
        "benchmark_reports": _report_hashes(checkpoint),
        "tensor_schema": schema,
        "files": [
            {
                "path": WEIGHTS_NAME,
                "size_bytes": len(weights),
                "sha256": hashlib.sha256(weights).hexdigest(),
            }
        ],
    }
    validate_manifest(manifest)
    raw_manifest = _canonical_json_bytes(manifest)
    if len(raw_manifest) > MAX_MANIFEST_BYTES:
        _fail(
            "bundle.size_limit",
            MANIFEST_NAME,
            f"manifest exceeds {MAX_MANIFEST_BYTES} bytes",
        )
    destination = Path(destination).expanduser().resolve()
    if destination.suffix.lower() != BUNDLE_EXTENSION:
        _fail(
            "bundle.invalid_extension",
            "destination",
            f"output path must end in {BUNDLE_EXTENSION}",
        )
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary_path = None
    try:
        with tempfile.NamedTemporaryFile(
            prefix=f".{destination.name}.",
            suffix=".tmp",
            dir=destination.parent,
            delete=False,
        ) as temporary:
            temporary_path = Path(temporary.name)
        with zipfile.ZipFile(
            temporary_path, "w", compression=zipfile.ZIP_STORED, allowZip64=True
        ) as archive:
            archive.writestr(_zip_info(MANIFEST_NAME), raw_manifest)
            archive.writestr(_zip_info(WEIGHTS_NAME), weights)
        if temporary_path.stat().st_size > MAX_BUNDLE_BYTES:
            _fail(
                "bundle.size_limit",
                "destination",
                f"bundle exceeds {MAX_BUNDLE_BYTES} bytes",
            )
        os.replace(temporary_path, destination)
        temporary_path = None
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)
    return verify_model_bundle(
        destination,
        development_allow_unsigned=True,
    )
