"""Independent logical oracle for a serial, single-block, ordered-prefix contract."""


MAX_LBAS = 16
MAX_EVENTS = 32
MAX_STATES = 256  # Refuse larger traces; never approximate an unconstrained result as PASS.


class History:
    def __init__(self, initial):
        if not 0 < len(initial) <= MAX_LBAS:
            raise ValueError("LBA geometry outside model bounds")
        self.width = len(initial[0])
        if not 0 < self.width <= 4096 or not all(
            isinstance(b, bytes) and len(b) == self.width for b in initial
        ):
            raise ValueError("expected equal-sized blocks of bytes")
        state = tuple(initial)
        self._live = {state}
        self._crash = {state}
        self.blocks = len(state)
        self.events = 0

    def observe(self, op, outcome, lba=None, value=None):
        """Append one caller observation; never infer 'rejected' from errno alone."""
        if self.events >= MAX_EVENTS or outcome not in ("ok", "uncertain", "rejected"):
            raise ValueError("history budget or outcome invalid")
        if op == "flush":
            if lba is not None or value is not None:
                raise ValueError("unexpected flush arguments")
            if outcome == "ok":
                self._crash = self._live.copy()  # Discard all pre-barrier prefixes.
        else:
            if op == "discard" and value is None:
                value = bytes(self.width)
            elif op != "write":
                raise ValueError("expected write with data or discard without data")
            if (not isinstance(lba, int) or not 0 <= lba < self.blocks or
                    not isinstance(value, bytes) or len(value) != self.width):
                raise ValueError("invalid logical write")
            if outcome != "rejected":
                changed = {s[:lba] + (value,) + s[lba + 1:] for s in self._live}
                live = self._live | changed if outcome == "uncertain" else changed
                crash = self._crash | live
                if len(crash) > MAX_STATES:
                    raise ValueError("too many candidate prefixes; incomplete coverage")
                self._live, self._crash = live, crash
        self.events += 1

    def accepts(self, observed, *, after_crash):
        """Check the entire observed vector so a skipped prefix cannot pass LBA by LBA."""
        return tuple(observed) in (self._crash if after_crash else self._live)
