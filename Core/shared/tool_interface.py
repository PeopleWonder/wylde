"""
Tool Interface Module - Standardized tool contract
CANONICAL SOURCE: core/shared/tool_interface.py
To update all service copies run: python core/shared/sync.py

Defines base classes and dataclasses for all tools in the system.
Every tool must conform to this interface to be discoverable via Tool Registry.
"""

from dataclasses import dataclass, asdict, field
from typing import Any, Dict, List, Optional
from datetime import datetime


# EXCEPTIONS
class ToolError(Exception):
    """Represents an error during tool execution."""

    def __init__(
        self, code: str, message: str, details: Optional[Dict[str, Any]] = None
    ):
        self.code = code
        self.message = message
        self.details = details or {}
        super().__init__(message)

    def to_dict(self) -> Dict[str, Any]:
        """Convert to dictionary for JSON serialization."""
        return {"code": self.code, "message": self.message, "details": self.details}


# DATA MODELS
@dataclass
class ToolParameter:
    """Defines a single input parameter for a tool."""

    name: str  # Parameter name
    type: str  # Type: string, number, boolean, array, object
    description: str  # Human-readable description
    required: bool = True  # Whether parameter is required
    default: Optional[Any] = None  # Default value if not required
    enum: Optional[List[Any]] = None  # Allowed values (if restricted)

    def to_dict(self) -> Dict[str, Any]:
        """Convert to dictionary."""
        return asdict(self)

    def validate(self, value: Any) -> bool:
        """Validate a value against this parameter definition."""
        if value is None:
            return not self.required

        # Type validation
        type_map: Dict[str, type | tuple[type, ...]] = {
            "string": str,
            "number": (int, float),
            "boolean": bool,
            "array": list,
            "object": dict,
        }

        if self.type in type_map and not isinstance(value, type_map[self.type]):
            return False

        # Enum validation
        if self.enum and value not in self.enum:
            return False

        return True


@dataclass
class ToolMetadata:
    """Describes a tool's capabilities, inputs, and requirements."""

    name: str  # Unique tool name (e.g., "scrape")
    description: str  # Human-readable description
    service_name: str  # Service providing this tool (e.g., "webcrawler")
    parameters: List[ToolParameter]  # Input parameter schema
    authentication: str = "none"  # Auth required (e.g., "api_key", "none")
    tags: List[str] = field(
        default_factory=list
    )  # Tags for discovery (e.g., ["web", "scraping"])
    deprecated: bool = False  # Whether tool is deprecated
    version: str = "1.0"  # Tool version

    def to_dict(self) -> Dict[str, Any]:
        """Convert to dictionary."""
        return {
            "name": self.name,
            "description": self.description,
            "service_name": self.service_name,
            "parameters": [p.to_dict() for p in self.parameters],
            "authentication": self.authentication,
            "tags": self.tags,
            "deprecated": self.deprecated,
            "version": self.version,
        }

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "ToolMetadata":
        """Create from dictionary."""
        parameters = [ToolParameter(**p) for p in data.get("parameters", [])]
        return cls(
            name=data["name"],
            description=data["description"],
            service_name=data["service_name"],
            parameters=parameters,
            authentication=data.get("authentication", "none"),
            tags=data.get("tags", []),
            deprecated=data.get("deprecated", False),
            version=data.get("version", "1.0"),
        )


@dataclass
class ToolContext:
    """Execution context for tool execution."""

    user_id: str  # User identifier
    session_id: str  # Session identifier
    trace_id: str  # Distributed trace ID
    timestamp: datetime = field(default_factory=datetime.now)  # Execution time
    metadata: Optional[Dict[str, Any]] = None  # Additional context

    def to_dict(self) -> Dict[str, Any]:
        """Convert to dictionary."""
        return {
            "user_id": self.user_id,
            "session_id": self.session_id,
            "trace_id": self.trace_id,
            "timestamp": self.timestamp.isoformat(),
            "metadata": self.metadata or {},
        }

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "ToolContext":
        """Create from dictionary."""
        raw_ts = data.get("timestamp")
        if isinstance(raw_ts, str):
            timestamp = datetime.fromisoformat(raw_ts)
        elif isinstance(raw_ts, datetime):
            timestamp = raw_ts
        else:
            timestamp = datetime.now()

        return cls(
            user_id=data["user_id"],
            session_id=data["session_id"],
            trace_id=data["trace_id"],
            timestamp=timestamp,
            metadata=data.get("metadata"),
        )


@dataclass
class ToolResult:
    """Standardized result from tool execution."""

    status: str  # "success" or "error"
    data: Any  # Result data (if success)
    metadata: Dict[str, Any]  # Metadata (execution time, etc.)
    trace_id: str  # Trace ID from context
    error: Optional[ToolError] = None  # Error details (if failure)

    def to_dict(self) -> Dict[str, Any]:
        """Convert to dictionary for JSON serialization."""
        result = {
            "status": self.status,
            "data": self.data,
            "metadata": self.metadata,
            "trace_id": self.trace_id,
        }

        if self.error:
            result["error"] = self.error.to_dict()

        return result

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "ToolResult":
        """Create from dictionary."""
        error = None
        if data.get("error"):
            error_data = data["error"]
            error = ToolError(
                code=error_data["code"],
                message=error_data["message"],
                details=error_data.get("details"),
            )

        return cls(
            status=data["status"],
            data=data.get("data"),
            metadata=data.get("metadata", {}),
            trace_id=data["trace_id"],
            error=error,
        )


# BASE INTERFACE
class ToolInterface:
    """Base class for all tools. All tools must inherit from this."""

    @property
    def metadata(self) -> ToolMetadata:
        """Return tool metadata."""
        raise NotImplementedError("Tool must implement metadata property")

    def execute(self, params: Dict[str, Any], context: ToolContext) -> ToolResult:
        """Execute the tool with given parameters and context."""
        raise NotImplementedError("Tool must implement execute method")

    def validate_params(self, params: Dict[str, Any]) -> bool:
        """Validate parameters against metadata schema."""
        raise NotImplementedError("Tool must implement validate_params method")


# EXAMPLE IMPLEMENTATIONS (for reference/testing)
class ExecutePythonTool(ToolInterface):
    """Example: Execute Python code tool."""

    @property
    def metadata(self) -> ToolMetadata:
        return ToolMetadata(
            name="execute_python",
            description="Execute Python code",
            service_name="tool-runner",
            parameters=[
                ToolParameter(
                    name="code",
                    type="string",
                    description="Python code to execute",
                    required=True,
                ),
                ToolParameter(
                    name="timeout",
                    type="number",
                    description="Execution timeout in seconds",
                    required=False,
                    default=30,
                ),
            ],
            authentication="none",
            tags=["code", "python", "execution"],
        )

    def execute(self, params: Dict[str, Any], context: ToolContext) -> ToolResult:
        try:
            self.validate_params(params)

            # Execute code (simplified - actual implementation would use subprocess)
            result = {"output": "Code executed successfully"}

            return ToolResult(
                status="success",
                data=result,
                metadata={"execution_time": 0.5},
                trace_id=context.trace_id,
            )
        except Exception as e:
            return ToolResult(
                status="error",
                data=None,
                metadata={},
                trace_id=context.trace_id,
                error=ToolError(code="EXECUTION_ERROR", message=str(e)),
            )

    def validate_params(self, params: Dict[str, Any]) -> bool:
        if "code" not in params or not isinstance(params["code"], str):
            raise ValueError("'code' parameter required and must be string")
        return True


class ScrapeTool(ToolInterface):
    """Example: Web scraping tool."""

    @property
    def metadata(self) -> ToolMetadata:
        return ToolMetadata(
            name="scrape",
            description="Scrape web page content",
            service_name="webcrawler",
            parameters=[
                ToolParameter(
                    name="url",
                    type="string",
                    description="URL to scrape",
                    required=True,
                ),
                ToolParameter(
                    name="selectors",
                    type="array",
                    description="CSS selectors to extract",
                    required=False,
                ),
            ],
            authentication="none",
            tags=["web", "scraping", "fetch"],
        )

    def execute(self, params: Dict[str, Any], context: ToolContext) -> ToolResult:
        try:
            self.validate_params(params)
            url = params["url"]

            # Scrape (simplified - actual implementation would use requests + BeautifulSoup)
            result = {"content": "Scraped content", "url": url}

            return ToolResult(
                status="success",
                data=result,
                metadata={"size_bytes": 1024},
                trace_id=context.trace_id,
            )
        except Exception as e:
            return ToolResult(
                status="error",
                data=None,
                metadata={},
                trace_id=context.trace_id,
                error=ToolError(code="SCRAPE_ERROR", message=str(e)),
            )

    def validate_params(self, params: Dict[str, Any]) -> bool:
        if "url" not in params or not isinstance(params["url"], str):
            raise ValueError("'url' parameter required and must be string")
        return True
