"""pytest.approx — tries to use the real _pytest.python_api when available,
falls back to a self-contained implementation matching pytest 9.x behaviour."""

from __future__ import annotations

import itertools
import sys


# If the real pytest source is on PYTHONPATH (e.g. during conformance testing),
# import everything from there so that assertion-rewriting isinstance checks
# against _pytest.python_api.ApproxBase still work.
#
# The shim's own _pytest/python_api.py has a _stub __getattr__ fallback that
# returns _Unsupported (not a real class) without raising ImportError, so we
# must verify that what we got is an actual type.
def _try_import_real_pytest():
    try:
        import _pytest.python_api as _real_api

        if not isinstance(getattr(_real_api, "ApproxBase", None), type):
            return False
        _g = sys.modules[__name__].__dict__
        for _name in (
            "ApproxBase",
            "ApproxDecimal",
            "ApproxMapping",
            "ApproxNumpy",
            "ApproxScalar",
            "ApproxSequenceLike",
            "approx",
            "_compare_approx",
            "_recursive_sequence_map",
        ):
            if hasattr(_real_api, _name):
                _g[_name] = getattr(_real_api, _name)
        # The shim's assertion/util.py imports _Approx as ApproxBase; keep the alias.
        _g["_Approx"] = _real_api.ApproxBase
        return True
    except Exception:
        return False


_USING_REAL_PYTEST = _try_import_real_pytest()
del _try_import_real_pytest

if not _USING_REAL_PYTEST:
    import math
    import pprint
    import sys
    from collections.abc import Collection, Mapping, Sized
    from decimal import Decimal
    from numbers import Complex

    def _isnan(x):
        # Only floats can be NaN, and NaN is the only float unequal to itself. Guard
        # on isinstance so we never call `!=` on an arbitrary object (e.g. a nested
        # approx), which would recurse back into this comparison. (isinstance avoids
        # importing `math`, which runs too early in the shim bootstrap.)
        return isinstance(x, float) and x != x

    def _as_numpy_array(obj):
        """Return an ndarray if the given object is implicitly convertible to
        ndarray and numpy is already imported, otherwise None."""
        np = sys.modules.get("numpy")
        if np is not None:
            # avoid infinite recursion on numpy scalars, which have __array__
            if np.isscalar(obj):
                return None
            elif isinstance(obj, np.ndarray):
                return obj
            elif hasattr(obj, "__array__") or hasattr("obj", "__array_interface__"):
                return np.asarray(obj)
        return None

    def _is_numpy_array(obj):
        return _as_numpy_array(obj) is not None

    def _is_sequence_like(expected):
        return (
            hasattr(expected, "__getitem__")
            and isinstance(expected, Sized)
            and not isinstance(expected, (str, bytes))
        )

    def _recursive_sequence_map(f, x):
        """Recursively map a function over a sequence of arbitrary depth."""
        if isinstance(x, (list, tuple)):
            seq_type = type(x)
            return seq_type(_recursive_sequence_map(f, xi) for xi in x)
        elif _is_sequence_like(x):
            return [_recursive_sequence_map(f, xi) for xi in x]
        else:
            return f(x)

    def _compare_approx(
        full_object,
        message_data,
        number_of_elements,
        different_ids,
        max_abs_diff,
        max_rel_diff,
    ):
        message_list = list(message_data)
        message_list.insert(0, ("Index", "Obtained", "Expected"))
        max_sizes = [0, 0, 0]
        for index, obtained, expected in message_list:
            max_sizes[0] = max(max_sizes[0], len(index))
            max_sizes[1] = max(max_sizes[1], len(obtained))
            max_sizes[2] = max(max_sizes[2], len(expected))
        explanation = [
            f"comparison failed. Mismatched elements: {len(different_ids)} / {number_of_elements}:",
            f"Max absolute difference: {max_abs_diff}",
            f"Max relative difference: {max_rel_diff}",
        ] + [
            f"{indexes:<{max_sizes[0]}} | {obtained:<{max_sizes[1]}} | {expected:<{max_sizes[2]}}"
            for indexes, obtained, expected in message_list
        ]
        return explanation

    class ApproxBase:
        """Provide shared utilities for making approximate comparisons between
        numbers or sequences of numbers."""

        # Tell numpy to use our `__eq__` operator instead of its.
        __array_ufunc__ = None
        __array_priority__ = 100

        def __init__(self, expected, rel=None, abs=None, nan_ok: bool = False) -> None:
            __tracebackhide__ = True
            self.expected = expected
            self.abs = abs
            self.rel = rel
            self.nan_ok = nan_ok
            self._check_type()

        def __repr__(self) -> str:
            raise NotImplementedError

        def _repr_compare(self, other_side) -> list:
            return [
                "comparison failed",
                f"Obtained: {other_side}",
                f"Expected: {self}",
            ]

        def __eq__(self, actual) -> bool:
            return all(a == self._approx_scalar(x) for a, x in self._yield_comparisons(actual))

        def __bool__(self):
            __tracebackhide__ = True
            raise AssertionError(
                "approx() is not supported in a boolean context.\nDid you mean: `assert a == approx(b)`?"
            )

        __hash__ = None  # type: ignore[assignment]

        def __ne__(self, actual) -> bool:
            return not (actual == self)

        def _approx_scalar(self, x):
            if isinstance(x, Decimal):
                return ApproxDecimal(x, rel=self.rel, abs=self.abs, nan_ok=self.nan_ok)
            return ApproxScalar(x, rel=self.rel, abs=self.abs, nan_ok=self.nan_ok)

        def _yield_comparisons(self, actual):
            """Yield all the pairs of numbers to be compared."""
            raise NotImplementedError

        def _check_type(self) -> None:
            """Raise a TypeError if the expected value is not a valid type."""

    class ApproxNumpy(ApproxBase):
        """Perform approximate comparisons where the expected value is numpy array."""

        def __repr__(self) -> str:
            list_scalars = _recursive_sequence_map(self._approx_scalar, self.expected.tolist())
            return f"approx({list_scalars!r})"

        def _repr_compare(self, other_side) -> list:
            def get_value_from_nested_list(nested_list, nd_index):
                value = nested_list
                for i in nd_index:
                    value = value[i]
                return value

            np_array_shape = self.expected.shape
            approx_side_as_seq = _recursive_sequence_map(
                self._approx_scalar, self.expected.tolist()
            )

            other_side_as_array = _as_numpy_array(other_side)
            assert other_side_as_array is not None

            if np_array_shape != other_side_as_array.shape:
                return [
                    "Impossible to compare arrays with different shapes.",
                    f"Shapes: {np_array_shape} and {other_side_as_array.shape}",
                ]

            number_of_elements = self.expected.size
            max_abs_diff = -math.inf
            max_rel_diff = -math.inf
            different_ids = []
            for index in itertools.product(*(range(i) for i in np_array_shape)):
                approx_value = get_value_from_nested_list(approx_side_as_seq, index)
                other_value = get_value_from_nested_list(other_side_as_array, index)
                if approx_value != other_value:
                    abs_diff = abs(approx_value.expected - other_value)
                    max_abs_diff = max(max_abs_diff, abs_diff)
                    if other_value == 0.0:
                        max_rel_diff = math.inf
                    else:
                        max_rel_diff = max(max_rel_diff, abs_diff / abs(other_value))
                    different_ids.append(index)

            message_data = [
                (
                    str(index),
                    str(get_value_from_nested_list(other_side_as_array, index)),
                    str(get_value_from_nested_list(approx_side_as_seq, index)),
                )
                for index in different_ids
            ]
            return _compare_approx(
                self.expected,
                message_data,
                number_of_elements,
                different_ids,
                max_abs_diff,
                max_rel_diff,
            )

        def __eq__(self, actual) -> bool:
            import numpy as np

            if not np.isscalar(actual):
                try:
                    actual = np.asarray(actual)
                except Exception as e:
                    raise TypeError(f"cannot compare '{actual}' to numpy.ndarray") from e

            if not np.isscalar(actual) and actual.shape != self.expected.shape:
                return False

            return super().__eq__(actual)

        def _yield_comparisons(self, actual):
            import numpy as np

            if np.isscalar(actual):
                for i in np.ndindex(self.expected.shape):
                    yield actual, self.expected[i].item()
            else:
                for i in np.ndindex(self.expected.shape):
                    yield actual[i].item(), self.expected[i].item()

    class ApproxMapping(ApproxBase):
        """Perform approximate comparisons where the expected value is a mapping
        with numeric values (the keys can be anything)."""

        def __repr__(self) -> str:
            return f"approx({ ({k: self._approx_scalar(v) for k, v in self.expected.items()})!r})"

        def _repr_compare(self, other_side) -> list:
            if len(self.expected) != len(other_side):
                return [
                    "Impossible to compare mappings with different sizes.",
                    f"Lengths: {len(self.expected)} and {len(other_side)}",
                ]

            if self.expected.keys() != other_side.keys():
                return [
                    "comparison failed.",
                    f"Mappings has different keys: expected {self.expected.keys()} but got {other_side.keys()}",
                ]

            approx_side_as_map = {k: self._approx_scalar(v) for k, v in self.expected.items()}

            number_of_elements = len(approx_side_as_map)
            max_abs_diff = -math.inf
            max_rel_diff = -math.inf
            different_ids = []
            for approx_key, approx_value in approx_side_as_map.items():
                other_value = other_side[approx_key]
                if approx_value != other_value:
                    if approx_value.expected is not None and other_value is not None:
                        try:
                            max_abs_diff = max(
                                max_abs_diff, abs(approx_value.expected - other_value)
                            )
                            if approx_value.expected == 0.0:
                                max_rel_diff = math.inf
                            else:
                                max_rel_diff = max(
                                    max_rel_diff,
                                    abs(
                                        (approx_value.expected - other_value)
                                        / approx_value.expected
                                    ),
                                )
                        except (ZeroDivisionError, TypeError):
                            pass
                    different_ids.append(approx_key)

            message_data = [
                (str(key), str(other_side[key]), str(approx_side_as_map[key]))
                for key in different_ids
            ]

            return _compare_approx(
                self.expected,
                message_data,
                number_of_elements,
                different_ids,
                max_abs_diff,
                max_rel_diff,
            )

        def __eq__(self, actual) -> bool:
            try:
                if set(actual.keys()) != set(self.expected.keys()):
                    return False
            except AttributeError:
                return False

            return super().__eq__(actual)

        def _yield_comparisons(self, actual):
            for k in self.expected.keys():
                yield actual[k], self.expected[k]

        def _check_type(self) -> None:
            __tracebackhide__ = True
            for key, value in self.expected.items():
                if isinstance(value, type(self.expected)):
                    msg = "pytest.approx() does not support nested dictionaries: key={!r} value={!r}\n  full mapping={}"
                    raise TypeError(msg.format(key, value, pprint.pformat(self.expected)))

    class ApproxSequenceLike(ApproxBase):
        """Perform approximate comparisons where the expected value is a sequence of numbers."""

        def __repr__(self) -> str:
            seq_type = type(self.expected)
            if seq_type not in (tuple, list):
                seq_type = list
            return f"approx({seq_type(self._approx_scalar(x) for x in self.expected)!r})"

        def _repr_compare(self, other_side) -> list:
            if len(self.expected) != len(other_side):
                return [
                    "Impossible to compare lists with different sizes.",
                    f"Lengths: {len(self.expected)} and {len(other_side)}",
                ]

            approx_side_as_map = _recursive_sequence_map(self._approx_scalar, self.expected)

            number_of_elements = len(approx_side_as_map)
            max_abs_diff = -math.inf
            max_rel_diff = -math.inf
            different_ids = []
            for i, (approx_value, other_value) in enumerate(
                zip(approx_side_as_map, other_side, strict=True)
            ):
                if approx_value != other_value:
                    try:
                        abs_diff = abs(approx_value.expected - other_value)
                        max_abs_diff = max(max_abs_diff, abs_diff)
                    # Ignore non-numbers for the diff calculations (#13012).
                    except TypeError:
                        pass
                    else:
                        if other_value == 0.0:
                            max_rel_diff = math.inf
                        else:
                            max_rel_diff = max(max_rel_diff, abs_diff / abs(other_value))
                    different_ids.append(i)
            message_data = [
                (str(i), str(other_side[i]), str(approx_side_as_map[i])) for i in different_ids
            ]

            return _compare_approx(
                self.expected,
                message_data,
                number_of_elements,
                different_ids,
                max_abs_diff,
                max_rel_diff,
            )

        def __eq__(self, actual) -> bool:
            try:
                if len(actual) != len(self.expected):
                    return False
            except TypeError:
                return False
            return super().__eq__(actual)

        def _yield_comparisons(self, actual):
            return zip(actual, self.expected, strict=True)

        def _check_type(self) -> None:
            __tracebackhide__ = True
            for index, x in enumerate(self.expected):
                if isinstance(x, type(self.expected)):
                    msg = "pytest.approx() does not support nested data structures: {!r} at index {}\n  full sequence: {}"
                    raise TypeError(msg.format(x, index, pprint.pformat(self.expected)))

    class ApproxScalar(ApproxBase):
        """Perform approximate comparisons where the expected value is a single number."""

        DEFAULT_ABSOLUTE_TOLERANCE = 1e-12
        DEFAULT_RELATIVE_TOLERANCE = 1e-6

        def __repr__(self) -> str:
            """Return a string communicating both the expected value and the
            tolerance for the comparison being made.

            For example, ``1.0 ± 1e-6``, ``(3+4j) ± 5e-6 ∠ ±180°``.
            """
            # Don't show a tolerance for values that aren't compared using
            # tolerances, i.e. non-numerics and infinities.
            if isinstance(self.expected, bool):
                return str(self.expected)
            if not isinstance(self.expected, (Complex, Decimal)):
                return str(self.expected)
            try:
                _abs_expected = abs(self.expected)
            except Exception:
                return str(self.expected)
            if math.isinf(_abs_expected):
                return str(self.expected)

            # If a sensible tolerance can't be calculated, self.tolerance will
            # raise a ValueError.  In this case, display '???'.
            try:
                if 1e-3 <= self.tolerance < 1e3:
                    vetted_tolerance = f"{self.tolerance:n}"
                else:
                    vetted_tolerance = f"{self.tolerance:.1e}"

                if (
                    isinstance(self.expected, Complex)
                    and self.expected.imag
                    and not math.isinf(self.tolerance)
                ):
                    vetted_tolerance += " ∠ ±180°"
            except ValueError:
                vetted_tolerance = "???"

            return f"{self.expected} ± {vetted_tolerance}"

        def __eq__(self, actual) -> bool:
            """Return whether the given value is equal to the expected value
            within the pre-specified tolerance."""

            def is_bool(val):
                # Check if val is a native bool or numpy bool.
                if isinstance(val, bool):
                    return True
                if np := sys.modules.get("numpy"):
                    return isinstance(val, np.bool_)
                return False

            asarray = _as_numpy_array(actual)
            if asarray is not None:
                # Call __eq__() manually to prevent infinite-recursion with
                # numpy<1.13.  See #3748.
                return all(self.__eq__(a) for a in asarray.flat)

            # Short-circuit exact equality, except for bool and np.bool_
            if is_bool(self.expected) and not is_bool(actual):
                return False
            elif actual == self.expected:
                return True

            # If either type is non-numeric, fall back to strict equality.
            if is_bool(self.expected) or not (
                isinstance(self.expected, (Complex, Decimal))
                and isinstance(actual, (Complex, Decimal))
            ):
                return False

            # Allow the user to control whether NaNs are considered equal to each
            # other or not.
            if math.isnan(abs(self.expected)):
                return self.nan_ok and math.isnan(abs(actual))

            # Infinity shouldn't be approximately equal to anything but itself
            if math.isinf(abs(self.expected)):
                return False

            # Return true if the two numbers are within the tolerance.
            result: bool = abs(self.expected - actual) <= self.tolerance
            return result

        __hash__ = None

        @property
        def tolerance(self):
            """Return the tolerance for the comparison."""

            def set_default(x, default):
                return x if x is not None else default

            absolute_tolerance = set_default(self.abs, self.DEFAULT_ABSOLUTE_TOLERANCE)

            if absolute_tolerance < 0:
                raise ValueError(f"absolute tolerance can't be negative: {absolute_tolerance}")
            if math.isnan(absolute_tolerance):
                raise ValueError("absolute tolerance can't be NaN.")

            # If the user specified an absolute tolerance but not a relative one,
            # just return the absolute tolerance.
            if self.rel is None:
                if self.abs is not None:
                    return absolute_tolerance

            relative_tolerance = set_default(self.rel, self.DEFAULT_RELATIVE_TOLERANCE) * abs(
                self.expected
            )

            if relative_tolerance < 0:
                raise ValueError(f"relative tolerance can't be negative: {relative_tolerance}")
            if math.isnan(relative_tolerance):
                raise ValueError("relative tolerance can't be NaN.")

            return max(relative_tolerance, absolute_tolerance)

    class ApproxDecimal(ApproxScalar):
        """Perform approximate comparisons where the expected value is a Decimal."""

        DEFAULT_ABSOLUTE_TOLERANCE = Decimal("1e-12")  # type: ignore[assignment]
        DEFAULT_RELATIVE_TOLERANCE = Decimal("1e-6")  # type: ignore[assignment]

        def __repr__(self) -> str:
            if isinstance(self.rel, float):
                rel = Decimal.from_float(self.rel)
            else:
                rel = self.rel

            if isinstance(self.abs, float):
                abs_ = Decimal.from_float(self.abs)
            else:
                abs_ = self.abs

            tol_str = "???"
            if rel is not None and Decimal("1e-3") <= rel <= Decimal("1e3"):
                tol_str = f"{rel:.1e}"
            elif abs_ is not None:
                tol_str = f"{abs_:.1e}"

            return f"{self.expected} ± {tol_str}"

    def approx(expected, rel=None, abs=None, nan_ok: bool = False) -> ApproxBase:
        """Assert that two numbers (or two ordered sequences of numbers) are equal to each other
        within some tolerance.

        Due to the :doc:`python:tutorial/floatingpoint`, numbers that we
        would intuitively expect to be equal are not always so::

            >>> 0.1 + 0.2 == 0.3
            False

        This problem is commonly encountered when writing tests, e.g. when making
        sure that floating-point values are what you expect them to be.  One way to
        deal with this problem is to assert that two floating-point numbers are
        equal to within some appropriate tolerance::

            >>> abs((0.1 + 0.2) - 0.3) < 1e-6
            True

        However, comparisons like this are tedious to write and difficult to
        understand.  Furthermore, absolute comparisons like the one above are
        usually discouraged because there's no tolerance that works well for all
        situations.  ``1e-6`` is good for numbers around ``1``, but too small for
        very big numbers and too big for very small ones.  It's better to express
        the tolerance as a fraction of the expected value, but relative comparisons
        like that are even more difficult to write correctly and concisely.

        The ``approx`` class performs floating-point comparisons using a syntax
        that's as intuitive as possible::

            >>> from pytest import approx
            >>> 0.1 + 0.2 == approx(0.3)
            True

        The same syntax also works for ordered sequences of numbers::

            >>> (0.1 + 0.2, 0.2 + 0.4) == approx((0.3, 0.6))
            True

        ``numpy`` arrays::

            >>> import numpy as np                                                          # doctest: +SKIP
            >>> np.array([0.1, 0.2]) + np.array([0.2, 0.4]) == approx(np.array([0.3, 0.6])) # doctest: +SKIP
            True

        And for a ``numpy`` array against a scalar::

            >>> import numpy as np                                         # doctest: +SKIP
            >>> np.array([0.1, 0.2]) + np.array([0.2, 0.1]) == approx(0.3) # doctest: +SKIP
            True

        Only ordered sequences are supported, because ``approx`` needs
        to infer the relative position of the sequences without ambiguity. This means
        ``sets`` and other unordered sequences are not supported.

        Finally, dictionary *values* can also be compared::

            >>> {'a': 0.1 + 0.2, 'b': 0.2 + 0.4} == approx({'a': 0.3, 'b': 0.6})
            True

        The comparison will be true if both mappings have the same keys and their
        respective values match the expected tolerances.

        **Tolerances**

        By default, ``approx`` considers numbers within a relative tolerance of
        ``1e-6`` (i.e. one part in a million) of its expected value to be equal.
        This treatment would lead to surprising results if the expected value was
        ``0.0``, because nothing but ``0.0`` itself is relatively close to ``0.0``.
        To handle this case less surprisingly, ``approx`` also considers numbers
        within an absolute tolerance of ``1e-12`` of its expected value to be
        equal.  Infinity and NaN are special cases.  Infinity is only considered
        equal to itself, regardless of the relative tolerance.  NaN is not
        considered equal to anything by default, but you can make it be equal to
        itself by setting the ``nan_ok`` argument to True.  (This is meant to
        facilitate comparing arrays that use NaN to mean "no data".)

        Both the relative and absolute tolerances can be changed by passing
        arguments to the ``approx`` constructor::

            >>> 1.0001 == approx(1)
            False
            >>> 1.0001 == approx(1, rel=1e-3)
            True
            >>> 1.0001 == approx(1, abs=1e-3)
            True

        If you specify ``abs`` but not ``rel``, the comparison will not consider
        the relative tolerance at all.  In other words, two numbers that are within
        the default relative tolerance of ``1e-6`` will still be considered unequal
        if they exceed the specified absolute tolerance.  If you specify both
        ``abs`` and ``rel``, the numbers will be considered equal if either
        tolerance is met::

            >>> 1 + 1e-8 == approx(1)
            True
            >>> 1 + 1e-8 == approx(1, abs=1e-12)
            False
            >>> 1 + 1e-8 == approx(1, rel=1e-6, abs=1e-12)
            True

        **Non-numeric types**

        You can also use ``approx`` to compare non-numeric types, or dicts and
        sequences containing non-numeric types, in which case it falls back to
        strict equality. This can be useful for comparing dicts and sequences that
        can contain optional values::

            >>> {"required": 1.0000005, "optional": None} == approx({"required": 1, "optional": None})
            True
            >>> [None, 1.0000005] == approx([None,1])
            True
            >>> ["foo", 1.0000005] == approx([None,1])
            False

        If you're thinking about using ``approx``, then you might want to know how
        it compares to other good ways of comparing floating-point numbers.  All of
        these algorithms are based on relative and absolute tolerances and should
        agree for the most part, but they do have meaningful differences:

        - ``math.isclose(a, b, rel_tol=1e-9, abs_tol=0.0)``:  True if the relative
          tolerance is met w.r.t. either ``a`` or ``b`` or if the absolute
          tolerance is met.  Because the relative tolerance is calculated w.r.t.
          both ``a`` and ``b``, this test is symmetric (i.e.  neither ``a`` nor
          ``b`` is a "reference value").  You have to specify an absolute tolerance
          if you want to compare to ``0.0`` because there is no tolerance by
          default.  More information: :py:func:`math.isclose`.

        - ``numpy.isclose(a, b, rtol=1e-5, atol=1e-8)``: True if the difference
          between ``a`` and ``b`` is less that the sum of the relative tolerance
          w.r.t. ``b`` and the absolute tolerance.  Because the relative tolerance
          is only calculated w.r.t. ``b``, this test is asymmetric and you can
          think of ``b`` as the reference value.  Support for comparing sequences
          is provided by :py:func:`numpy.allclose`.  More information:
          :std:doc:`numpy:reference/generated/numpy.isclose`.

        - ``unittest.TestCase.assertAlmostEqual(a, b)``: True if ``a`` and ``b``
          are within an absolute tolerance of ``1e-7``.  No relative tolerance is
          considered , so this function is not appropriate for very large or very
          small numbers.  Also, it's only available in subclasses of ``unittest.TestCase``
          and it's ugly because it doesn't follow PEP8.  More information:
          :py:meth:`unittest.TestCase.assertAlmostEqual`.

        - ``a == pytest.approx(b, rel=1e-6, abs=1e-12)``: True if the relative
          tolerance is met w.r.t. ``b`` or if the absolute tolerance is met.
          Because the relative tolerance is only calculated w.r.t. ``b``, this test
          is asymmetric and you can think of ``b`` as the reference value.  In the
          special case that you explicitly specify an absolute tolerance but not a
          relative tolerance, only the absolute tolerance is considered.

        .. note::

            ``approx`` can handle numpy arrays, but we recommend the
            specialised test helpers in :std:doc:`numpy:reference/routines.testing`
            if you need support for comparisons, NaNs, or ULP-based tolerances.

            To match strings using regex, you can use
            `Matches <https://github.com/asottile/re-assert#re_assertmatchespattern-str-args-kwargs>`_
            from the
            `re_assert package <https://github.com/asottile/re-assert>`_.


        .. note::

            Unlike built-in equality, this function considers
            booleans unequal to numeric zero or one. For example::

               >>> 1 == approx(True)
               False

        .. warning::

           .. versionchanged:: 3.2

           In order to avoid inconsistent behavior, :py:exc:`TypeError` is
           raised for ``>``, ``>=``, ``<`` and ``<=`` comparisons.
           The example below illustrates the problem::

               assert approx(0.1) > 0.1 + 1e-10  # calls approx(0.1).__gt__(0.1 + 1e-10)
               assert 0.1 + 1e-10 > approx(0.1)  # calls approx(0.1).__lt__(0.1 + 1e-10)

           In the second example one expects ``approx(0.1).__le__(0.1 + 1e-10)``
           to be called. But instead, ``approx(0.1).__lt__(0.1 + 1e-10)`` is used to
           comparison. This is because the call hierarchy of rich comparisons
           follows a fixed behavior. More information: :py:meth:`object.__ge__`

        .. versionchanged:: 3.7.1
           ``approx`` raises ``TypeError`` when it encounters a dict value or
           sequence element of non-numeric type.

        .. versionchanged:: 6.1.0
           ``approx`` falls back to strict equality for non-numeric types instead
           of raising ``TypeError``.
        """
        __tracebackhide__ = True

        cls: type[ApproxBase]
        if isinstance(expected, Decimal):
            cls = ApproxDecimal
        elif isinstance(expected, Mapping):
            cls = ApproxMapping
        elif _is_numpy_array(expected):
            expected = _as_numpy_array(expected)
            cls = ApproxNumpy
        elif _is_sequence_like(expected):
            cls = ApproxSequenceLike
        elif isinstance(expected, Collection) and not isinstance(expected, (str, bytes)):
            msg = f"pytest.approx() only supports ordered sequences, but got: {expected!r}"
            raise TypeError(msg)
        else:
            cls = ApproxScalar

        return cls(expected, rel, abs, nan_ok)

    _Approx = ApproxBase
