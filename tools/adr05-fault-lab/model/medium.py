"""Bounded physical-medium model. All offsets, lengths and short counts are bytes."""

from dataclasses import dataclass
import errno

# Enough for the small captured format-4 fixture and two complete 4 KiB record blocks.
MAX_IMAGE = 2 * 1024 * 1024
MAX_TRANSFER = 128 * 1024
MAX_OPS = 32


def in_range(offset, length, capacity):
    if not (0 <= offset < capacity and 0 < length <= capacity - offset):
        raise ValueError(f"invalid range {offset}+{length} (capacity {capacity})")


@dataclass(frozen=True)
class Fault:
    index: int  # 1-based across all accepted calls, never relative to operation kind
    op: str
    offset: int
    length: int
    action: str
    arg: int = 0


# The arg is errno for *_error, the new offset for misdirect or the count for short.
ACTIONS = {
    "pre_error": {"read", "write", "discard", "sync"},
    "post_error": {"write", "discard", "sync"},
    "short": {"read", "write"},
    "stale": {"read"},
    "misdirect": {"read", "write"},
    "disappear": {"read", "write", "discard", "sync"},
    "false_ack": {"sync"},
}


class Medium:
    def __init__(self, initial, faults=()):
        if not 0 < len(initial) <= MAX_IMAGE or len(faults) > MAX_OPS:
            raise ValueError("image or plan exceeds model bounds")
        self.durable = bytes(initial)
        self.visible = bytearray(initial)
        self.pending = {}  # write index -> (physical offset, bytes actually accepted)
        self.index = 0
        self.offline = False
        self.plan = {}
        for rule in faults:
            if not (1 <= rule.index <= MAX_OPS) or rule.index in self.plan:
                raise ValueError("fault needs a unique, in-budget operation index")
            if rule.op not in ACTIONS.get(rule.action, set()):
                raise ValueError("unsupported fault/operation combination")
            self._geometry(rule.op, rule.offset, rule.length)
            if rule.action == "misdirect":
                in_range(rule.arg, rule.length, len(initial))
            elif rule.action == "short" and not 0 <= rule.arg < rule.length:
                raise ValueError("short count must be less than request length")
            elif rule.action.endswith("_error") and rule.arg <= 0:
                raise ValueError("positive errno required")
            self.plan[rule.index] = rule
        self.hits = dict.fromkeys(self.plan, 0)

    def _geometry(self, op, offset, length):
        if op == "sync" and offset == length == 0:
            return
        if op not in ("read", "write", "discard") or length > MAX_TRANSFER:
            raise ValueError("unsupported operation or transfer length")
        in_range(offset, length, len(self.durable))

    def _next(self, op, offset, length):
        self._geometry(op, offset, length)
        if self.index == MAX_OPS:
            raise ValueError("operation limit exceeded")
        self.index += 1
        if self.offline:
            raise OSError(errno.ENODEV, "device already gone")
        f = self.plan.get(self.index)
        if f is None or (f.op, f.offset, f.length) != (op, offset, length):
            return None
        self.hits[f.index] += 1
        if f.action == "disappear":
            self.offline = True
            raise OSError(errno.ENODEV, "device disappeared")
        if f.action == "pre_error":
            raise OSError(f.arg, "before operation")
        return f

    def read(self, offset, length):
        f = self._next("read", offset, length)
        if f and f.action == "misdirect":
            offset = f.arg
        if f and f.action == "short":
            length = f.arg
        src = self.durable if f and f.action == "stale" else self.visible
        return bytes(src[offset:offset + length])

    def _modify(self, op, offset, data):
        f = self._next(op, offset, len(data))
        if f and f.action == "misdirect":
            offset = f.arg
        if f and f.action == "short":
            data = data[:f.arg]
        self.visible[offset:offset + len(data)] = data
        self.pending[self.index] = (offset, bytes(data))
        if f and f.action == "post_error":
            raise OSError(f.arg, "after applying operation")
        return len(data)

    def write(self, offset, data):
        return self._modify("write", offset, data)

    def discard(self, offset, length):
        self._geometry("discard", offset, length)
        return self._modify("discard", offset, bytes(length))

    def sync(self):
        f = self._next("sync", 0, 0)
        if f and f.action == "false_ack":  # explicit violation of the sync contract
            return
        self.durable = bytes(self.visible)
        self.pending.clear()
        if f and f.action == "post_error":
            raise OSError(f.arg, "sync completed but completion failed")

    def crash_image(self, fragments=()):
        """Return a new image, applying (pending index, start in write, length) in given order."""
        if len(fragments) > MAX_OPS * 8:
            raise ValueError("too many fragments")
        image = bytearray(self.durable)
        for index, start, length in fragments:
            if index not in self.pending:
                raise ValueError(f"{index} is not a pending write")
            offset, data = self.pending[index]
            in_range(start, length, len(data))
            image[offset + start:offset + start + length] = data[start:start + length]
        return bytes(image)
