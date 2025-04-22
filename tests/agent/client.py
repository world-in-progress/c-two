import os
import httpx
import asyncio
from typing import Optional
from contextlib import AsyncExitStack

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

from anthropic import Anthropic
from dotenv import load_dotenv

CLAUDE_3_5 = 'claude-3-5-sonnet-20241022'
CLAUDE_3_7 = 'claude-3-7-sonnet-20250219'
CURRENT_MODEL = CLAUDE_3_5

load_dotenv(dotenv_path=os.path.join(os.path.dirname(__file__), ".env"))

class MCPClient:
    def __init__(self):
        # Initialize session and client objects
        self.session: Optional[ClientSession] = None
        self.exit_stack = AsyncExitStack()
        
        proxy = os.getenv("HTTPS_PROXY") or os.getenv("HTTP_PROXY")
        if proxy:
            print(f'Using proxy: {proxy}')
        
        self.anthropic = Anthropic(
            http_client=httpx.Client()
        )

    async def connect_to_server(self, server_script_path: str):
        """Connect to an MCP server
        
        Args:
            server_script_path: Path to the server script (.py or .js)
        """
        is_python = server_script_path.endswith('.py')
        is_js = server_script_path.endswith('.js')
        if not (is_python or is_js):
            raise ValueError("Server script must be a .py or .js file")
            
        command = "python" if is_python else "node"
        server_params = StdioServerParameters(
            command=command,
            args=[server_script_path],
            env=None
        )
        
        stdio_transport = await self.exit_stack.enter_async_context(stdio_client(server_params))
        self.stdio, self.write = stdio_transport
        self.session = await self.exit_stack.enter_async_context(ClientSession(self.stdio, self.write))
        
        await self.session.initialize()
        
        # List available tools
        response = await self.session.list_tools()
        tools = response.tools
        print("\nConnected to server with tools:", [tool.name for tool in tools])

    async def process_query(self, query: str) -> str:
        """Process a query using Claude and available tools"""
        messages = [
            {
                "role": "user",
                "content": query
            }
        ]

        response = await self.session.list_tools()
        available_tools = [{
            "name": tool.name,
            "description": tool.description,
            "input_schema": tool.inputSchema
        } for tool in response.tools]

        # Initial Claude API call
        response = self.anthropic.messages.create(
            model=CURRENT_MODEL,
            max_tokens=1000,
            messages=messages,
            tools=available_tools
        )

        final_text = []
        tool_calls_made = 0
        max_tool_calls = 1000
        
        while tool_calls_made < max_tool_calls:
            has_tool_call = False

            for content in response.content:
                if content.type == 'text':
                    final_text.append(content.text)
                elif content.type == 'tool_use':
                    has_tool_call = True
                    tool_calls_made += 1
                    tool_id = content.id
                    tool_name = content.name
                    tool_args = content.input
                    
                    # Execute tool call
                    result = await self.session.call_tool(tool_name, tool_args)
                    # final_text.append(f'\n[Using tool: {tool_name}]\n')
                    final_text.append(f"\n[Using tool: {tool_name}]\nInput: {tool_args}\nResult: {result.content}")

                    # Add both the assistant's tool call and the result to the message history
                    messages.append({
                        "role": "assistant",
                        "content": [
                            {"type": "text", "text": f"I'll use the {tool_name} tool."},
                            {"type": "tool_use", "id": tool_id, "name": tool_name, "input": tool_args}
                        ]
                    })
                    
                    messages.append({
                        "role": "user", 
                        "content": [
                            {"type": "tool_result", "tool_use_id": tool_id, "content": result.content}
                        ]
                    })
                    
                    # Break after processing the first tool call
                    break

            # If no tool calls were made, we're done
            if not has_tool_call:
                break
            
            # Get next response from Claude
            response = self.anthropic.messages.create(
                model=CURRENT_MODEL,
                max_tokens=1000,
                messages=messages,
                tools=available_tools
            )

        return "\n".join(final_text)

    async def chat_loop(self):
        """Run an interactive chat loop"""
        print("\nMCP Client Started!")
        print("Type your queries or 'quit' to exit.")
        
        while True:
            try:
                query = input("\nQuery: ").strip()
                
                if query.lower() == 'quit':
                    break
                    
                response = await self.process_query(query)
                print("\n" + response)
                    
            except Exception as e:
                print(f"\nError: {str(e)}")
    
    async def cleanup(self):
        """Clean up resources"""
        await self.exit_stack.aclose()

async def main():
    if len(sys.argv) < 2:
        print("Usage: python client.py <path_to_server_script>")
        sys.exit(1)
        
    client = MCPClient()
    try:
        await client.connect_to_server(sys.argv[1])
        await client.chat_loop()
    finally:
        await client.cleanup()

if __name__ == "__main__":
    import sys
    asyncio.run(main())