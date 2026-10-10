"""Small contract shared by the two idle-pool regression subprocesses."""
import c_two as cc


@cc.crm(namespace='test.idle_pool', version='0.1.0')
class Echo:
    def echo(self, value: str) -> str:
        ...
