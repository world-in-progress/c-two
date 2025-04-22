import os
import sys
from mcp.server.fastmcp import FastMCP
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '..')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../../src/')))
import c_two as cc
import compo

mcp = FastMCP('Grid', instructions=cc.mcp.CC_INSTRUCTION)

cc.mcp.register_mcp_tools_from_compo_module(mcp, compo)

if __name__ == '__main__':
    mcp.run()
