# tests/unit/test_held_result.py
import warnings
import pytest


class TestHeldResultBasic:
    """HeldResult lifecycle: access, release, double-release."""

    def test_access_value(self):
        from c_two.crm.transferable import HeldResult
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

    def test_release_cb_exception_swallowed(self):
        from c_two.crm.transferable import HeldResult
        def bad_cb():
            raise RuntimeError('boom')
        hr = HeldResult('err', release_cb=bad_cb)
        hr.release()  # should not raise
        with pytest.raises(Exception):
            _ = hr.value


class TestHoldFunction:
    """cc.hold() wraps a bound ICRM method to inject _c2_buffer='hold'."""

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
