import c_two as cc
# Define ICRM ###########################################################
@cc.icrm
class IHello:
    def greeting(self, name: str) -> str:
        ...