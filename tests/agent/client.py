import os
import sys
import json
import httpx
import asyncio
from typing import Optional
from dotenv import load_dotenv
from anthropic import Anthropic, NOT_GIVEN, NotGiven
from openai import AsyncOpenAI, OpenAI, NOT_GIVEN as OpenAI_NOT_GIVEN
from contextlib import AsyncExitStack

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

OLLAMA_3_2 = 'llama3.2:latest'
DEEPSEEK_CHAT = 'deepseek-chat'
CLAUDE_3_5 = 'claude-3-5-sonnet-20241022'
CLAUDE_3_7 = 'claude-3-7-sonnet-20250219'
CURRENT_MODEL = DEEPSEEK_CHAT

MODEL_PROVIDER_OLLAMA = 'ollama'
MODEL_PROVIDER_DEEPSEEK = 'deepseek'
MODEL_PROVIDER_ANTHROPIC = 'anthropic'
CURRENT_PROVIDER = MODEL_PROVIDER_DEEPSEEK

def ollama_default_model() -> str:
    """Get the default model for Ollama"""
    return OLLAMA_3_2

def deepseek_default_model() -> str:
    """Get the default model for DeepSeek"""
    return DEEPSEEK_CHAT

def anthropic_default_model() -> str:
    """Get the default model for Anthropic"""
    return CLAUDE_3_5

load_dotenv(dotenv_path=os.path.join(os.path.dirname(__file__), '.env'))

class MCPClient:
    def __init__(self):
        # Initialize session and client objects
        self.session: Optional[ClientSession] = None
        self.exit_stack = AsyncExitStack()
        
        # Initialize Anthropic client
        self.anthropic = Anthropic(
            http_client=httpx.Client()
        )
        
        # Initialize DeepSeek client (using OpenAI compatible interface)
        self.deepseek = AsyncOpenAI(
            api_key=os.getenv('DEEPSEEK_API_KEY'),
            base_url='https://api.deepseek.com/v1',
        )
        
        # Initialize Ollama client (using OpenAI compatible interface)
        self.ollama = OpenAI(
            base_url='http://localhost:11434/v1',
            api_key='ollama',
        )
        
        # Initialize available tools list
        self.available_tools: list[dict[str, any]] | NotGiven = NOT_GIVEN

    def set_model_provider(self, provider: str, model: str = None):
        """Set the model provider and model to use
        
        Args:
            provider: Provider name ('anthropic', 'deepseek', or 'ollama')
            model: Specific model name to use
        """
        global CURRENT_PROVIDER, CURRENT_MODEL
        supported_providers = [MODEL_PROVIDER_ANTHROPIC, MODEL_PROVIDER_DEEPSEEK, MODEL_PROVIDER_OLLAMA]
        if provider not in supported_providers:
            raise ValueError(f"Unsupported model provider: {provider}. Supported: {', '.join(supported_providers)}")
        
        CURRENT_PROVIDER = provider
        
        if model:
            CURRENT_MODEL = model
        else:
            if provider == MODEL_PROVIDER_DEEPSEEK:
                CURRENT_MODEL = deepseek_default_model()
            elif provider == MODEL_PROVIDER_ANTHROPIC:
                CURRENT_MODEL = anthropic_default_model()
            elif provider == MODEL_PROVIDER_OLLAMA:
                CURRENT_MODEL = ollama_default_model()
        
        print(f'Switched to model provider: {CURRENT_PROVIDER}, model: {CURRENT_MODEL}')

    async def connect_to_server(self, server_script_path: str):
        """Connect to an MCP server
        
        Args:
            server_script_path: Path to the server script (.py)
        """
        if not (server_script_path.endswith('.py')):
            raise ValueError('Server script must be a .py file')
            
        server_params = StdioServerParameters(
            command='python',
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
        print('\nConnected to server with tools:', [tool.name for tool in tools])
        
        # Store available tools
        self.available_tools = [{
            'name': tool.name,
            'description': tool.description,
            'input_schema': tool.inputSchema
        } for tool in tools]

    async def process_query(self, query: str, accumulated_text: str | None = None) -> str:
        """Process a query using selected model and available tools"""
        # Function to record text output, either printing or accumulating
        def record(text: str):
            nonlocal accumulated_text
            if accumulated_text is None:
                print(text, end='', flush=True)
            else:
                accumulated_text += text
        
        messages = [
            {
                'role': 'user',
                'content': query
            }
        ]
        
        if CURRENT_PROVIDER == MODEL_PROVIDER_ANTHROPIC:
            return await self._process_anthropic(messages, record, accumulated_text)
        elif CURRENT_PROVIDER == MODEL_PROVIDER_DEEPSEEK:
            return await self._process_openai(messages, record, accumulated_text)
        elif CURRENT_PROVIDER == MODEL_PROVIDER_OLLAMA:
            return await self._process_openai(messages, record, accumulated_text)
        else:
            raise ValueError(f'Unsupported model provider: {CURRENT_PROVIDER}')
            
    async def _process_anthropic(self, messages, record_fn, accumulated_text):
        """Process query using Anthropic model"""
        tool_call = True
        while tool_call:
            tool_call = False
            with self.anthropic.messages.stream(
                model=CURRENT_MODEL,
                max_tokens=1024,
                messages=messages,
                tools=self.available_tools,
            ) as stream:
                for text in stream.text_stream:
                    record_fn(text)
                
                for content in stream.get_final_message().content:
                    # Execute tool call
                    if content.type == 'tool_use':
                        tool_call = True
                        tool_id = content.id
                        tool_name = content.name
                        tool_args = content.input

                        record_fn(f'\n\n[Using tool: {tool_name}]\n')
                        result = await self.session.call_tool(tool_name, tool_args)
                        record_fn(f'[Tool result received]\n\n')

                        # Add both the assistant's tool call and the result to the message history
                        messages.append({
                            'role': 'assistant',
                            'content': [
                                {'type': 'text', 'text': f"I'll use the {tool_name} tool."},
                                {'type': 'tool_use', 'id': tool_id, 'name': tool_name, 'input': tool_args}
                            ]
                        })
                        
                        messages.append({
                            'role': 'user', 
                            'content': [
                                {'type': 'tool_result', 'tool_use_id': tool_id, 'content': result.content}
                            ]
                        })
                    
                record_fn('\n')
        return accumulated_text
    
    async def _process_openai(self, messages, record_fn, accumulated_text):
        """Process query using DeepSeek model"""
        tool_call = True
        
        # Convert MCP tools format to OpenAI compatible format (used by DeepSeek)
        openai_tools: list[dict[str, any]] = OpenAI_NOT_GIVEN
        if self.available_tools is not NOT_GIVEN:
            openai_tools = []
            for tool in self.available_tools:
                openai_tools.append({
                    'type': 'function',
                    'function': {
                        'name': tool['name'],
                        'description': tool['description'],
                        'parameters': tool['input_schema']
                    }
                })
        
        # Convert to OpenAI format messages (DeepSeek compatible)
        openai_messages = []
        for msg in messages:
            if isinstance(msg['content'], str):
                openai_messages.append({
                    'role': msg['role'],
                    'content': msg['content']
                })
            else:
                # Handle complex content types
                content_str = ''
                for part in msg['content']:
                    if part['type'] == 'text':
                        content_str += part['text'] + '\n'
                    elif part['type'] == 'tool_result':
                        content_str += f'Tool result: {part["content"]}\n'
                
                openai_messages.append({
                    'role': msg['role'],
                    'content': content_str.strip()
                })
            
        while tool_call:
            tool_call = False
            
            try:
                # Use DeepSeek API (OpenAI compatible) to start streaming generation
                response = await self.deepseek.chat.completions.create(
                    model=CURRENT_MODEL,
                    messages=openai_messages,
                    temperature=0.7,
                    tools=openai_tools,
                    stream=True
                )
                
                response_text = ''
                assistant_message = {'role': 'assistant', 'content': ''}
                tool_calls_info = []
                current_tool_call = None
                
                # Process streaming response
                async for chunk in response:
                    # Process text content
                    if chunk.choices[0].delta.content:
                        text = chunk.choices[0].delta.content
                        response_text += text
                        assistant_message['content'] += text
                        record_fn(text)
                    
                    # Process tool calls
                    if chunk.choices[0].delta.tool_calls:
                        for tool_call_delta in chunk.choices[0].delta.tool_calls:
                            index = tool_call_delta.index
                            
                            # Initialize or get current tool call info
                            while len(tool_calls_info) <= index:
                                tool_calls_info.append({'name': '', 'arguments': ''})
                            
                            current_tool_call = tool_calls_info[index]
                            
                            # Update tool call name and arguments
                            if hasattr(tool_call_delta.function, 'name') and tool_call_delta.function.name:
                                current_tool_call['name'] = tool_call_delta.function.name
                            
                            if hasattr(tool_call_delta.function, 'arguments') and tool_call_delta.function.arguments:
                                current_tool_call['arguments'] += tool_call_delta.function.arguments
                
                # If there are tool calls, process them
                if tool_calls_info and any(tc['name'] for tc in tool_calls_info):
                    tool_call = True
                    
                    # Add assistant message to history
                    openai_messages.append(assistant_message)
                    
                    # Process each tool call
                    for tool_info in tool_calls_info:
                        if tool_info['name']:
                            try:
                                tool_name = tool_info['name']
                                tool_args = json.loads(tool_info['arguments'])
                                
                                record_fn(f'\n\n[Using tool: {tool_name}]\n')
                                result = await self.session.call_tool(tool_name, tool_args)
                                record_fn(f'[Tool result received]\n\n')
                                
                                # Add tool call result to message history
                                openai_messages.append({
                                    'role': 'user',
                                    'content': f"Tool '{tool_name}' result: {result.content}"
                                })
                                
                            except Exception as e:
                                record_fn(f'\n[Error calling tool: {str(e)}]\n')
                                openai_messages.append({
                                    'role': 'user',
                                    'content': f'Error calling tool: {str(e)}'
                                })
                
            except Exception as e:
                record_fn(f'\nError with DeepSeek API: {str(e)}\n')
                break
            
            record_fn('\n')
        
        return accumulated_text
    
    async def process_query_stream(self, query: str):
        """Process a query using the selected model with streaming output
        
        Args:
            query: The user query to process
            
        Yields:
            Strings of text as they are generated by the model or tool results
        """
        # Choose different processing methods based on the selected provider
        if CURRENT_PROVIDER == MODEL_PROVIDER_ANTHROPIC:
            async for text in self._process_anthropic_stream(query):
                yield text
        elif CURRENT_PROVIDER == MODEL_PROVIDER_DEEPSEEK:
            async for text in self._process_openai_stream(query):
                yield text
        elif CURRENT_PROVIDER == MODEL_PROVIDER_OLLAMA:
            async for text in self._process_openai_stream(query):
                yield text
        else:
            yield f'Unsupported model provider: {CURRENT_PROVIDER}'
    
    async def _process_anthropic_stream(self, query: str):
        """Stream processing using Anthropic model"""
        messages = [{'role': 'user', 'content': query}]
        
        tool_call = True
        while tool_call:
            tool_call = False
            with self.anthropic.messages.stream(
                model=CURRENT_MODEL,
                max_tokens=1024,
                messages=messages,
                tools=self.available_tools,
            ) as stream:
                # Stream text tokens as they're generated
                for text in stream.text_stream:
                    yield text
                
                for content in stream.get_final_message().content:
                    # Execute tool call
                    if content.type == 'tool_use':
                        tool_call = True
                        tool_id = content.id
                        tool_name = content.name
                        tool_args = content.input
                    
                        # Notify about tool usage
                        yield f'\n[Using tool: {tool_name}]\n'
                        
                        # Call the tool
                        result = await self.session.call_tool(tool_name, tool_args)
                        
                        # Yield tool result notification
                        yield f'\n[Tool result received]\n'

                        # Add both the assistant's tool call and the result to the message history
                        messages.append({
                            'role': 'assistant',
                            'content': [
                                {'type': 'text', 'text': f"I'll use the {tool_name} tool."},
                                {'type': 'tool_use', 'id': tool_id, 'name': tool_name, 'input': tool_args}
                            ]
                        })
                        
                        messages.append({
                            'role': 'user', 
                            'content': [
                                {'type': 'tool_result', 'tool_use_id': tool_id, 'content': result.content}
                            ]
                        })

        # Signal completion of the entire response
        yield '\n'
    
    async def _process_openai_stream(self, query: str):
        """Stream processing using DeepSeek model"""
        async def inner_generator():
            messages = [{'role': 'user', 'content': query}]
            
            # Convert MCP tools format to OpenAI compatible format
            openai_tools: list[dict[str, any]] | NotGiven = NOT_GIVEN
            if self.available_tools is not OpenAI_NOT_GIVEN:
                openai_tools = []
                for tool in self.available_tools:
                    openai_tools.append({
                        'type': 'function',
                        'function': {
                            'name': tool['name'],
                            'description': tool['description'],
                            'parameters': tool['input_schema']
                        }
                    })
                
            openai_messages = []
            for msg in messages:
                if isinstance(msg['content'], str):
                    openai_messages.append({
                        'role': msg['role'],
                        'content': msg['content']
                    })
                else:
                    # Process complex content
                    content_str = ''
                    for part in msg['content']:
                        if part['type'] == 'text':
                            content_str += part['text'] + '\n'
                        elif part['type'] == 'tool_result':
                            content_str += f"Tool result: {part['content']}\n"
                    
                    openai_messages.append({
                        'role': msg['role'],
                        'content': content_str.strip()
                    })
            
            tool_call = True
            while tool_call:
                tool_call = False
                try:
                    # Use DeepSeek API (OpenAI compatible) to start streaming generation
                    response = await self.deepseek.chat.completions.create(
                        model=CURRENT_MODEL,
                        messages=openai_messages,
                        temperature=0.7,
                        tools=openai_tools,
                        stream=True
                    )
                    
                    response_text = ''
                    assistant_message = {'role': 'assistant', 'content': ''}
                    tool_calls_info = []
                    
                    # Process streaming response
                    async for chunk in response:
                        # Process text content
                        if chunk.choices[0].delta.content:
                            text = chunk.choices[0].delta.content
                            response_text += text
                            assistant_message['content'] += text
                            
                            yield text
                        
                        # Process tool calls
                        if chunk.choices[0].delta.tool_calls:
                            for tool_call_delta in chunk.choices[0].delta.tool_calls:
                                index = tool_call_delta.index
                                
                                # Initialize or get current tool call info
                                while len(tool_calls_info) <= index:
                                    tool_calls_info.append({'name': '', 'arguments': ''})
                                
                                current_tool_call = tool_calls_info[index]
                                
                                # Update tool call name and arguments
                                if hasattr(tool_call_delta.function, 'name') and tool_call_delta.function.name:
                                    current_tool_call['name'] = tool_call_delta.function.name
                                
                                if hasattr(tool_call_delta.function, 'arguments') and tool_call_delta.function.arguments:
                                    current_tool_call['arguments'] += tool_call_delta.function.arguments
                    
                    # If there are tool calls, process them
                    if tool_calls_info and any(tc['name'] for tc in tool_calls_info):
                        tool_call = True
                        
                        # Add assistant message to history
                        openai_messages.append(assistant_message)
                        
                        # Process each tool call
                        for tool_info in tool_calls_info:
                            if tool_info['name']:
                                try:
                                    tool_name = tool_info['name']
                                    tool_args = json.loads(tool_info['arguments'])
                                    
                                    # Notify about
                                    yield f'\n[Using tool: {tool_name}]\n'
                                    result = await self.session.call_tool(tool_name, tool_args)
                                    yield f'\n[Tool result received]\n'
                                    
                                    openai_messages.append({
                                        'role': 'user',
                                        'content': f"Tool '{tool_name}' result: {result.content}"
                                    })
                                    
                                except Exception as e:
                                    yield f'\n[Error calling tool: {str(e)}]\n'
                                    openai_messages.append({
                                        'role': 'user',
                                        'content': f'Error calling tool: {str(e)}'
                                    })
                    
                except Exception as e:
                    yield f'\nError with DeepSeek API: {str(e)}\n'
                    break
                
                yield '\n'

        async for text in inner_generator():
            yield text
    
    async def chat_loop(self):
        """Run an interactive chat loop"""
        print('\nMCP Client Started!')
        print(f'Currently using: {CURRENT_PROVIDER} model {CURRENT_MODEL}')
        print("Type your queries, 'q' to exit, or:")
        print("  'use anthropic' - Switch to Anthropic")
        print("  'use deepseek' - Switch to DeepSeek")
        print("  'use ollama' - Switch to local Ollama")
        
        try:
            while True:
                try:
                    query = input('\nQuery: ').strip()
                    
                    if query.lower() == 'q':
                        break
                    elif query.lower() == 'use anthropic':
                        self.set_model_provider(MODEL_PROVIDER_ANTHROPIC)
                        continue
                    elif query.lower() == 'use deepseek':
                        self.set_model_provider(MODEL_PROVIDER_DEEPSEEK)
                        continue
                    elif query.lower() == 'use ollama':
                        self.set_model_provider(MODEL_PROVIDER_OLLAMA)
                        continue
                    
                    print('\n', end='', flush=True)
                    await self.process_query(query)
                    print('\n', end='', flush=True)
                        
                except Exception as e:
                    print(f'\nError: {str(e)}')
        except KeyboardInterrupt:
            print('\nExiting chat loop.')
            self.cleanup()
    
    async def cleanup(self):
        """Clean up resources"""
        await self.exit_stack.aclose()

async def main():
        
    client = MCPClient()
    try:
        if len(sys.argv) == 2:
            await client.connect_to_server(sys.argv[1])
        await client.chat_loop()
    finally:
        await client.cleanup()

if __name__ == '__main__':
    asyncio.run(main())
