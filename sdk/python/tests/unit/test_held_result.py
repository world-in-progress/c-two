# tests/unit/test_held_result.py
import sys
import traceback
import warnings
import pytest


class TestHeldResultBasic:
    """HeldResult lifecycle: access, release, double-release."""

    def test_access_value(self):
        from c_two.crm.transferable import Held, HeldResult
        assert Held is HeldResult
        hr = HeldResult(42, release_cb=None)
        assert hr.value == 42

    def test_release_clears_value(self):
        from c_two.crm.transferable import HeldResult
        released = []
        hr = HeldResult('data', release_cb=lambda: released.append(True))
        hr.release()
        assert released == [True]
        with pytest.raises(Exception):
            _ = hr.value

    def test_double_release_is_noop(self):
        from c_two.crm.transferable import HeldResult
        count = []
        hr = HeldResult(99, release_cb=lambda: count.append(1))
        hr.release()
        hr.release()
        assert len(count) == 1

    def test_context_manager(self):
        from c_two.crm.transferable import HeldResult
        released = []
        with HeldResult('ctx', release_cb=lambda: released.append(True)) as held:
            assert held.value == 'ctx'
        assert released == [True]
        with pytest.raises(Exception):
            _ = held.value

    def test_context_manager_preserves_body_and_release_errors(self):
        from c_two.crm.transferable import HeldResult

        body_error = ValueError('body failed')
        prior = KeyError('body context')
        explicit_cause = LookupError('body cause')

        def release():
            raise RuntimeError('cleanup failed')

        with pytest.raises(ValueError, match='body failed') as raised:
            with HeldResult('value', release_cb=release):
                try:
                    raise prior
                except KeyError:
                    raise body_error from explicit_cause

        assert raised.value is body_error
        assert body_error.__cause__ is explicit_cause
        assert isinstance(body_error.__context__, RuntimeError)
        assert str(body_error.__context__) == 'cleanup failed'
        assert body_error.__context__.__context__ is prior
        if sys.version_info >= (3, 11):
            assert 'cleanup failed' in ''.join(traceback.format_exception(body_error))

    def test_none_release_cb(self):
        from c_two.crm.transferable import HeldResult
        hr = HeldResult('no-shm', release_cb=None)
        hr.release()
        with pytest.raises(Exception):
            _ = hr.value

    def test_del_warns_if_not_released(self):
        from c_two.crm.transferable import HeldResult
        hr = HeldResult('leak', release_cb=lambda: None)
        with warnings.catch_warnings(record=True) as w:
            warnings.simplefilter('always')
            hr.__del__()
            assert len(w) == 1
            assert 'HeldResult' in str(w[0].message)
            assert issubclass(w[0].category, ResourceWarning)

    def test_del_no_warn_if_already_released(self):
        from c_two.crm.transferable import HeldResult
        hr = HeldResult('ok', release_cb=lambda: None)
        hr.release()
        with warnings.catch_warnings(record=True) as w:
            warnings.simplefilter('always')
            hr.__del__()
            assert len(w) == 0

    def test_del_reports_pending_native_cleanup_after_release_failure(self):
        from c_two.crm.transferable import HeldResult

        class Response:
            is_released = True
            has_pending_release = True
            abandoned = 0

            def release(self):
                raise RuntimeError('native cleanup failed')

            def _abandon_after_failed_release(self):
                self.abandoned += 1
                self.has_pending_release = False

        response = Response()
        held = HeldResult('value')
        held._native_response = response
        held._released = True
        held._release_attempted = True
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter('always')
            held.__del__()

        assert response.abandoned == 1
        assert len(caught) == 2
        assert 'pending native cleanup after release() failed' in str(caught[0].message)
        assert 'final release failed: native cleanup failed' in str(caught[1].message)

    def test_del_reports_prior_release_rejected_before_invalidation(self):
        from c_two.crm.transferable import HeldResult

        class Response:
            is_released = False
            has_pending_release = False

            def release(self):
                raise BufferError('another export is active')

            def _abandon_after_failed_release(self):
                pass

        held = HeldResult('value')
        held._native_response = Response()
        with pytest.raises(BufferError, match='another export'):
            held.release()
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter('always')
            held.__del__()

        assert len(caught) == 2
        assert 'release() did not complete' in str(caught[0].message)
        assert 'without release()' not in str(caught[0].message)

    def test_release_cb_exception_is_propagated_after_owner_is_closed(self):
        from c_two.crm.transferable import HeldResult
        def bad_cb():
            raise RuntimeError('boom')
        hr = HeldResult('err', release_cb=bad_cb)
        with pytest.raises(RuntimeError, match='boom'):
            hr.release()
        with pytest.raises(Exception):
            _ = hr.value

    def test_unsafe_buffer_exposes_held_memoryview_until_release(self):
        from c_two.crm.transferable import HeldResult

        raw = memoryview(b'abc')
        hr = HeldResult('value', release_cb=raw.release, buffer=raw)

        assert bytes(hr.unsafe_buffer) == b'abc'
        assert not hasattr(type(hr), 'buffer')
        assert hr.value == 'value'

        hr.release()

        with pytest.raises(Exception):
            _ = hr.unsafe_buffer

    def test_release_does_not_invalidate_external_owner_by_default(self):
        from c_two.crm.transferable import HeldResult

        class Owner:
            invalidated = False

            def invalidate(self):
                self.invalidated = True

        owner = Owner()
        HeldResult(owner, release_cb=None).release()
        assert owner.invalidated is False

    def test_invalidation_failure_does_not_skip_transport_release(self):
        from c_two.crm.transferable import HeldResult

        events = []

        def invalidate(_value):
            events.append('invalidate')
            raise RuntimeError('invalidate failed')

        def release():
            events.append('release')

        held = HeldResult(
            'value',
            release_cb=release,
            invalidate_cb=invalidate,
        )

        with pytest.raises(RuntimeError, match='invalidate failed'):
            held.release()

        assert events == ['invalidate', 'release']
        with pytest.raises(RuntimeError, match='no longer accessible'):
            _ = held.value

    def test_manual_callback_errors_keep_primary_chain_and_run_once(self):
        from c_two.crm.transferable import HeldResult

        original = LookupError('original cause')
        original_context = KeyError('original context')
        calls = []

        def invalidate(_value):
            calls.append('invalidate')
            try:
                raise original_context
            except KeyError:
                raise ValueError('invalidator failed') from original

        def release():
            calls.append('release')
            try:
                raise OSError('cleanup context')
            except OSError:
                raise RuntimeError('cleanup failed')

        held = HeldResult('value', release_cb=release, invalidate_cb=invalidate)
        with pytest.raises(ValueError, match='invalidator failed') as raised:
            held.release()
        assert raised.value.__cause__ is original
        assert isinstance(raised.value.__context__, RuntimeError)
        assert str(raised.value.__context__) == 'cleanup failed'
        assert isinstance(raised.value.__context__.__context__, OSError)
        assert raised.value.__context__.__context__.__context__ is original_context
        if sys.version_info >= (3, 11):
            assert 'cleanup failed' in ''.join(traceback.format_exception(raised.value))
            assert any('cleanup failed' in note for note in raised.value.__notes__)
        held.release()
        assert calls == ['invalidate', 'release']


class TestHoldFunction:
    """cc.hold() wraps a bound CRM method to inject _c2_buffer='hold'."""

    def test_hold_injects_c2_buffer(self):
        from c_two.crm.transferable import hold

        class FakeProxy:
            def compute(self, x, **kwargs):
                return kwargs

        proxy = FakeProxy()
        wrapped = hold(proxy.compute)
        result = wrapped(42)
        assert result['_c2_buffer'] == 'hold'

    def test_hold_rejects_unbound_function(self):
        from c_two.crm.transferable import hold

        def standalone(x):
            return x

        with pytest.raises(TypeError, match='bound'):
            hold(standalone)

    def test_hold_rejects_non_callable(self):
        from c_two.crm.transferable import hold

        with pytest.raises(TypeError):
            hold(42)

    def test_hold_preserves_args(self):
        from c_two.crm.transferable import hold

        class FakeProxy:
            def compute(self, a, b, **kwargs):
                return (a, b, kwargs.get('_c2_buffer'))

        proxy = FakeProxy()
        wrapped = hold(proxy.compute)
        result = wrapped(1, 2)
        assert result == (1, 2, 'hold')
